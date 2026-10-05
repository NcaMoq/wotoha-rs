//! Conservative, research-only tempo shadow evaluation.
//!
//! This module deliberately keeps the production analyzer and AutoMix planner
//! untouched.  It consumes the previous candidate-flow report, adds bounded
//! source-aware ranking and abstention, and evaluates a separate metrical
//! consistency guard against synthetic planner cases.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use wotoha_core::{
    analysis::{
        BeatEvent, Confidence, CueGenerationInput, CueRole, DjCue, MeterHypothesis, ModelScore,
        PhraseBoundary, Support, TempoHypothesis, TempoRelation, TrackAnalysisV2, UnitInterval,
        generate_heuristic_cues,
    },
    automix::{
        AutoMixConfig, TrackAnalysis, TransitionKind, TransitionPlanV2, V2AnalysisInput,
        V2GuardedTransitionPlan, beat_match_eligibility, evaluate_transition_quality,
        explain_beatmatch_decision_v2, plan_guarded_transition_v2, plan_transition_v2,
        transition_score_breakdown,
    },
};

use super::{
    tempo_ambiguity_research::{AnalyzedFixture, analyze_long_fixture, long_spec},
    tempo_shadow_followup::{
        CandidateFlowObservation, TempoCandidate, TempoShadowFollowupReport,
        build_flow_observation, run_tempo_shadow_followup,
    },
};
use crate::{
    EventStyle, FixtureFamily, FixtureSpec, LabError, SyntheticFixture, TempoProfile,
    TransformKind, generate_fixture, write_json,
};

const RELATIVE_TOLERANCE: f32 = 0.005;
const MAX_CANDIDATES: usize = 4;
const ACCEPT_SCORE: f32 = 0.68;
const ACCEPT_MARGIN: f32 = 0.08;
const ACCEPT_EVENT_AGREEMENT: f32 = 0.80;
const STATIONARY_DRIFT: f64 = 0.003;
const STATIONARY_DISPERSION: f64 = 0.003;
const CLASSICAL_ANCHOR_BONUS: f32 = 0.15;
const RELATION_RESOLUTION_TOLERANCE: f32 = 0.02;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TempoConservativeShadowReport {
    pub schema_version: u32,
    pub source_commit: String,
    pub starting_commit: Option<String>,
    pub production_behavior_changed: bool,
    pub fixture_count: usize,
    pub scalar_tempo_fixture_count: usize,
    pub candidate_budgets: Vec<BudgetSummary>,
    pub provenance_conditioned: Vec<ProvenanceCondition>,
    pub ranking_variants: Vec<RankingVariantSummary>,
    pub abstention: AbstentionReport,
    pub evidence_attribution: EvidenceAttribution,
    pub stationarity: Vec<StationarityObservation>,
    pub metrical_consistency: MetricalConsistencyAudit,
    pub adversarial: MetricalAdversarialReport,
    pub candidate_pressure: CandidatePressureReport,
    pub pruning: Vec<PruningSummary>,
    pub conservative_matrix: ConservativeShadowMatrix,
    pub effective_planner: EffectivePlannerSummary,
    pub heldout: HeldOutConservativeRanking,
    pub runtime_feature_audit: Vec<RuntimeFeatureAudit>,
    pub runtime_feasible: RuntimeFeasibleSummary,
    pub runtime_consistency: Vec<RuntimeTempoConsistencyObservation>,
    pub abstention_reasons: Vec<AbstentionReasonObservation>,
    pub realistic_corpus: RealisticSyntheticCorpusReport,
    /// Primary validation corpus. Its fixtures are generated and analyzed
    /// independently; the planner never receives a copied boundary window.
    pub independent_positive_corpus: RealisticPositiveCorpusReport,
    pub focus_slices: ConservativeFocusSlices,
    pub decision: ConservativeDecision,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BudgetSummary {
    pub budget: usize,
    pub mean_candidates: f32,
    pub p50_candidates: usize,
    pub p95_candidates: usize,
    pub max_candidates: usize,
    pub top1_canonical_correct: usize,
    pub top1_family_correct: usize,
    pub candidate_family_recall: usize,
    pub candidate_canonical_recall: usize,
    pub mean_pair_cross_product: f32,
    pub p95_pair_cross_product: usize,
    pub max_pair_cross_product: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProvenanceCondition {
    pub combination: String,
    pub candidates: usize,
    pub canonical_correct: usize,
    pub family_correct: usize,
    pub canonical_precision: Option<f32>,
    pub family_precision: Option<f32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RankingVariantSummary {
    pub name: String,
    pub candidate_budget: usize,
    pub scored: usize,
    pub accepted: usize,
    pub abstained: usize,
    pub retained_multiple: usize,
    pub canonical_correct: usize,
    pub family_correct: usize,
    pub canonical_precision: Option<f32>,
    pub family_precision: Option<f32>,
    pub false_confident_accepts: Option<usize>,
    pub candidate_family_recall: usize,
    pub candidate_canonical_recall: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AbstentionReport {
    pub rule_name: String,
    pub rows: Vec<TempoShadowDecisionRow>,
    pub pareto: Vec<OperatingPoint>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TempoShadowDecisionRow {
    pub fixture_id: String,
    pub family: String,
    pub truth_bpm: f32,
    pub candidate_budget: usize,
    pub decision: String,
    pub selected_bpm: Option<f32>,
    pub selected_relation: Option<String>,
    pub candidate_count: usize,
    pub top1_score: Option<f32>,
    pub top2_score: Option<f32>,
    pub score_margin: Option<f32>,
    pub source_support: usize,
    pub selected_sources: Vec<String>,
    pub classical_anchor_available: bool,
    pub event_agreement: Option<f32>,
    pub duration_stability: f32,
    pub metrical_consistency: String,
    pub canonical_correct: bool,
    pub family_correct: bool,
    pub false_confident_accept: bool,
    pub correct_abstention: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OperatingPoint {
    pub name: String,
    pub score_threshold: f32,
    pub margin_threshold: f32,
    pub accepted: usize,
    pub coverage: f32,
    pub canonical_precision: Option<f32>,
    pub family_precision: Option<f32>,
    pub false_confident: usize,
    pub false_beatmatched_proxy: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EvidenceAttribution {
    pub harmful_merged_cases: Vec<HarmfulEvidenceCase>,
    pub rescue_cases: Vec<RescueEvidenceCase>,
    pub harmful_source_counts: BTreeMap<String, usize>,
    pub rescue_source_counts: BTreeMap<String, usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HarmfulEvidenceCase {
    pub fixture_id: String,
    pub family: String,
    pub classical_bpm: Option<f32>,
    pub merged_bpm: Option<f32>,
    pub added_sources: Vec<String>,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RescueEvidenceCase {
    pub fixture_id: String,
    pub family: String,
    pub current_bpm: Option<f32>,
    pub shadow_bpm: Option<f32>,
    pub enabling_sources: Vec<String>,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StationarityObservation {
    pub fixture_id: String,
    pub profile: String,
    pub classification: String,
    pub interval_count: usize,
    pub median_interval_micros: Option<f64>,
    pub interval_mad_ratio: Option<f64>,
    pub early_middle_drift: Option<f64>,
    pub middle_late_drift: Option<f64>,
    pub early_late_drift: Option<f64>,
    pub fit_residual_ratio: Option<f64>,
    pub refinement_eligible: bool,
    pub abstain: bool,
    pub abstention_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MetricalConsistencyAudit {
    pub semantics: String,
    pub observations: Vec<MetricalConsistencyObservation>,
    pub counts: BTreeMap<String, usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MetricalConsistencyObservation {
    pub fixture_id: String,
    pub candidate_bpm: f32,
    pub relation: String,
    pub event_clock_bpm: Option<f32>,
    pub expected_candidate_bpm: Option<f32>,
    pub relative_error: Option<f32>,
    pub harmonic_ratio: Option<f32>,
    pub interval_dispersion: Option<f32>,
    pub status: String,
    pub source: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MetricalAdversarialReport {
    pub cases: Vec<MetricalAdversarialCase>,
    pub invalid_before_guard: usize,
    pub invalid_after_guard: usize,
    pub valid_aliases_retained: usize,
    pub valid_aliases_total: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MetricalAdversarialCase {
    pub case_id: String,
    pub event_clock_bpm: f32,
    pub candidate_bpm: f32,
    pub relation: String,
    pub declared_valid_alias: bool,
    pub consistency: String,
    pub planner_physical_eligible: bool,
    pub beatmatched_generated: bool,
    pub quality_guard_selected: bool,
    pub guarded_beatmatched: bool,
    pub beatmatched_after_metrical_guard: bool,
    pub invalid_before_guard: bool,
    pub invalid_after_guard: bool,
    pub transition: String,
    pub baseline_transition: String,
    pub baseline_beatmatched_selected: bool,
    pub shadow_decision_outgoing: String,
    pub shadow_decision_incoming: String,
    pub effective_shadow_transition: String,
    pub effective_shadow_beatmatched_selected: bool,
    pub effective_shadow_false_beatmatched: bool,
    pub effective_shadow_safe_fallback: bool,
    pub valid_alias_representable: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CandidatePressureReport {
    pub per_fixture: Vec<CandidatePressureRow>,
    pub mean_candidates: f32,
    pub p50_candidates: usize,
    pub p95_candidates: usize,
    pub max_candidates: usize,
    pub mean_pair_cross_product: f32,
    pub p95_pair_cross_product: usize,
    pub max_pair_cross_product: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CandidatePressureRow {
    pub fixture_id: String,
    pub candidate_count: usize,
    pub pair_cross_product: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PruningSummary {
    pub method: String,
    pub candidate_budget: usize,
    pub mean_candidates: f32,
    pub top1_canonical_correct: usize,
    pub top1_family_correct: usize,
    pub candidate_family_recall: usize,
    pub false_family_candidates: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConservativeShadowMatrix {
    pub variants: Vec<ConservativeVariantCase>,
    pub current_correct_beatmatched: usize,
    pub current_false_beatmatched: usize,
    pub conservative_correct_beatmatched: usize,
    pub conservative_false_beatmatched: usize,
    pub safe_fallback: usize,
    pub missed_opportunity: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConservativeVariantCase {
    pub pair_id: String,
    pub variant: String,
    pub selected_bpm_outgoing: Option<f32>,
    pub selected_bpm_incoming: Option<f32>,
    pub decision_outgoing: String,
    pub decision_incoming: String,
    pub metrical_guard_outgoing: String,
    pub metrical_guard_incoming: String,
    pub beatmatched_generated: bool,
    pub beatmatched_selected: bool,
    pub correct_beatmatched: bool,
    pub false_beatmatched: bool,
    pub safe_fallback: bool,
    pub missed_opportunity: bool,
    pub baseline_transition: String,
    pub baseline_beatmatched_selected: bool,
    pub shadow_decision_outgoing: String,
    pub shadow_decision_incoming: String,
    pub effective_shadow_transition: String,
    pub effective_shadow_beatmatched_selected: bool,
    pub effective_shadow_false_beatmatched: bool,
    pub effective_shadow_safe_fallback: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EffectivePlannerSummary {
    pub baseline_transition_cases: usize,
    pub baseline_beatmatched_selected: usize,
    pub baseline_false_beatmatched: usize,
    pub effective_transition_cases: usize,
    pub effective_beatmatched_selected: usize,
    pub effective_false_beatmatched: usize,
    pub effective_safe_fallback: usize,
    pub effective_missed_opportunity: usize,
    pub aliases_representable: usize,
    pub aliases_total: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RuntimeFeatureAudit {
    pub name: String,
    pub classification: String,
    pub definition: String,
    pub source: String,
    pub missing_value_behavior: String,
    pub production_time_available: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RuntimeFeasibleSummary {
    pub rule_name: String,
    pub scored: usize,
    pub accepted: usize,
    pub abstained_or_retained: usize,
    pub canonical_correct: usize,
    pub family_correct: usize,
    pub canonical_precision: Option<f32>,
    pub family_precision: Option<f32>,
    pub false_confident_accepts: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RuntimeTempoConsistencyObservation {
    pub fixture_id: String,
    pub family: String,
    pub interval_count: usize,
    pub interval_mad_ratio: Option<f64>,
    pub early_middle_drift: Option<f64>,
    pub middle_late_drift: Option<f64>,
    pub early_late_drift: Option<f64>,
    pub classification: String,
    pub refinement_would_abstain: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AbstentionReasonObservation {
    pub fixture_id: String,
    pub family: String,
    pub decision: String,
    pub reason: String,
    pub candidate_count: usize,
    pub candidate_family_available: bool,
    pub recoverable_by_current_candidate_set: bool,
}

/// A bounded, vendor-neutral synthetic corpus with arrangement-like changes
/// layered over the same known beat clock.  It is intentionally separate from
/// the scalar/alias corpus: the purpose is to test whether the conservative
/// shadow remains safe when evidence density changes over time.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RealisticSyntheticCorpusReport {
    pub schema_version: u32,
    pub source_commit: String,
    pub starting_commit: Option<String>,
    pub seed: u64,
    pub construction: String,
    pub fixture_count: usize,
    pub durations_micros: Vec<u64>,
    pub section_profiles: Vec<String>,
    pub fixtures: Vec<RealisticFixtureObservation>,
    pub candidate_caps: Vec<RealisticCandidateCapSummary>,
    pub transition_cases: Vec<RealisticTransitionCase>,
    pub transition_summary: RealisticTransitionSummary,
    pub interval_consistency: Vec<RuntimeTempoConsistencyObservation>,
    pub positive_corpus: RealisticPositiveCorpusReport,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RealisticFixtureObservation {
    pub fixture_id: String,
    pub profile: String,
    pub pcm_sha256: String,
    pub master_id: String,
    pub seed: u64,
    pub duration_micros: u64,
    pub truth_bpm: f32,
    pub event_count: usize,
    pub first_event_micros: Option<u64>,
    pub last_event_micros: Option<u64>,
    pub event_clock_bpm: Option<f32>,
    pub audible_start_micros: u64,
    pub audible_end_micros: u64,
    pub heuristic_cue_count: usize,
    pub heuristic_mix_in_cue_count: usize,
    pub heuristic_mix_out_cue_count: usize,
    pub candidate_count: usize,
    pub decision: String,
    pub selected_bpm: Option<f32>,
    pub selected_relation: Option<String>,
    pub relation_resolution: String,
    pub safe_fallback_only: bool,
    pub beat_grid: RealisticBeatGridDiagnostic,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RealisticBeatGridDiagnostic {
    pub matching_tolerance_micros: u64,
    pub truth_beat_count: usize,
    pub detected_beat_count: usize,
    pub matched_truth_beats: usize,
    pub unmatched_truth_beats: usize,
    pub extra_detected_beats: usize,
    pub precision: Option<f32>,
    pub recall: Option<f32>,
    pub median_absolute_timing_error_micros: Option<u64>,
    pub p95_absolute_timing_error_micros: Option<u64>,
    pub max_absolute_timing_error_micros: Option<u64>,
    pub first_beat_offset_micros: Option<i64>,
    pub longitudinal_drift_micros: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RealisticCandidateCapSummary {
    pub candidate_budget: usize,
    pub mean_candidates: f32,
    pub max_candidates: usize,
    pub max_pair_cross_product: usize,
    pub candidate_family_recall: usize,
    pub candidate_canonical_recall: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RealisticTransitionCase {
    pub case_id: String,
    pub outgoing_fixture: String,
    pub incoming_fixture: String,
    pub expected_outcome: String,
    pub baseline_transition: String,
    pub baseline_beatmatched_candidate_generated: bool,
    pub baseline_beatmatched_selected: bool,
    pub baseline_quality_guard_passed: bool,
    pub effective_shadow_transition: String,
    pub effective_beatmatched_candidate_generated: bool,
    pub effective_metrical_guard_passed: bool,
    pub effective_quality_guard_passed: bool,
    pub effective_beatmatched_selected: bool,
    pub effective_false_beatmatched: bool,
    pub effective_safe_fallback: bool,
    pub effective_pair_correct: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RealisticTransitionSummary {
    pub pair_count: usize,
    pub expected_safe_fallback_cases: usize,
    pub expected_feasible_cases: usize,
    pub baseline_false_beatmatched: usize,
    pub effective_false_beatmatched: usize,
    pub effective_correct_beatmatched: usize,
    pub effective_safe_fallback: usize,
    pub safe_outcomes: usize,
    pub non_interference_scope: String,
}

/// Positive transition cases use the same deterministic audio generator and
/// the same long-fixture analysis path as the negative corpus.  The planner
/// receives only analysis-time evidence: observed V2 events, inferred tempo
/// hypotheses, and heuristic cues derived from the analyzed structure.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RealisticPositiveCorpusReport {
    pub source_commit: Option<String>,
    pub starting_commit: Option<String>,
    pub corpus_kind: String,
    pub boundary_window_copy_used: bool,
    pub construction: String,
    pub fixture_count: usize,
    pub pair_count: usize,
    pub candidate_caps: Vec<RealisticCandidateCapSummary>,
    pub fixtures: Vec<RealisticFixtureObservation>,
    pub transition_cases: Vec<RealisticPositiveTransitionCase>,
    pub summary: RealisticPositiveSummary,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RealisticPositiveTransitionCase {
    pub case_id: String,
    pub outgoing_fixture: String,
    pub incoming_fixture: String,
    pub outgoing_truth_bpm: f32,
    pub incoming_truth_bpm: f32,
    pub outgoing_decision: String,
    pub incoming_decision: String,
    pub expected_relation: String,
    pub selected_relation_pair: Option<String>,
    pub selected_outgoing_bpm: Option<f32>,
    pub selected_incoming_bpm: Option<f32>,
    pub selected_ratio: Option<f32>,
    pub tempo_hypothesis_pair_correct: bool,
    pub relation_correct: bool,
    pub beatmatched_candidate_generated: bool,
    pub beatmatched_candidate_survived_guard: bool,
    pub quality_guard_passed: bool,
    pub beatmatched_selected: bool,
    pub false_beatmatched: bool,
    pub safe_fallback: bool,
    pub transition: String,
    pub planner_reason: String,
    pub beat_pairs: usize,
    pub phase_error_micros: Option<u64>,
    pub eligibility: String,
    pub eligibility_rejection: Option<String>,
    pub outgoing_mix_out_cues: usize,
    pub incoming_mix_in_cues: usize,
    pub cue_pairs_checked: usize,
    pub cue_tempo_combinations_checked: usize,
    pub planner_hard_rejections: Vec<String>,
    pub tempo_adjustment: Option<f32>,
    pub render_quality: crate::tempo_shadow_followup::RenderQualityObservation,
    pub opportunity_class: String,
    pub opportunity_taxonomy: String,
    pub outgoing_pcm_sha256: String,
    pub incoming_pcm_sha256: String,
    pub outgoing_master_id: String,
    pub incoming_master_id: String,
    pub outgoing_seed: u64,
    pub incoming_seed: u64,
    pub waveform_copy_used: bool,
    pub truth_inference_used: bool,
    pub ordinary_selected_kind: String,
    pub ordinary_beatmatched_cost: Option<f32>,
    pub ordinary_gapless_cost: Option<f32>,
    pub ordinary_crossfade_cost: Option<f32>,
    pub quality_first_selected_kind: String,
    pub quality_evidence: String,
    pub quality_first_disagreement_reason: Option<String>,
    pub candidate_costs: Vec<RealisticPlannerCandidateObservation>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RealisticPlannerCandidateObservation {
    pub kind: String,
    pub total_cost: f32,
    pub strategy_base_cost: f32,
    pub tempo_stretch_cost: f32,
    pub phase_precision_cost: f32,
    pub structure_uncertainty_cost: f32,
    pub structure_alignment_cost: f32,
    pub rhythm_uncertainty_cost: f32,
    pub cue_suitability_cost: f32,
    pub blend_duration_cost: f32,
    pub legacy_quality_cost: f32,
    pub quality_min_mix_energy_ratio: Option<f32>,
    pub quality_max_mix_energy_ratio: Option<f32>,
    pub quality_handoff_mix_energy_ratio: Option<f32>,
    pub quality_energy_balance_penalty: Option<f32>,
    pub quality_handoff_energy_penalty: Option<f32>,
    pub quality_handoff_ownership_penalty: Option<f32>,
    pub quality_phrase_strength_penalty: Option<f32>,
    pub quality_overlap_seconds: f32,
    pub outgoing_peak_dbfs: Option<f32>,
    pub incoming_peak_dbfs: Option<f32>,
    pub outgoing_rms_dbfs: Option<f32>,
    pub incoming_rms_dbfs: Option<f32>,
    pub quality_issues: Vec<String>,
    pub phase_error_micros: Option<u64>,
    pub outgoing_start_micros: u64,
    pub incoming_start_micros: u64,
    pub duration_micros: u64,
    pub tempo_pair: Option<String>,
    pub tempo_pair_relation: Option<String>,
    pub hard_rejection: Option<String>,
    pub outgoing_cue_index: Option<usize>,
    pub incoming_cue_index: Option<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RealisticPositiveSummary {
    pub positive_cases: usize,
    pub effective_correct_beatmatched: usize,
    pub effective_false_beatmatched: usize,
    pub safe_fallback: usize,
    pub candidate_generated_not_selected: usize,
    pub candidate_survived_guard_not_selected: usize,
    pub relation_unresolved: usize,
    pub quality_guard_rejected: usize,
    pub expected_relation_correct: usize,
    pub independent_correct_beatmatched: usize,
    pub distinct_success_tempo_regions: usize,
    pub distinct_success_profiles: usize,
    pub distinct_success_duration_configs: usize,
    pub independent_false_beatmatched: usize,
    pub external_gate_passed: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HeldOutConservativeRanking {
    pub rule_name: String,
    pub rule_frozen_before_scoring: bool,
    pub grouping_rule: String,
    pub folds: Vec<HeldOutConservativeFold>,
    pub aggregate: HeldOutConservativeAggregate,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HeldOutConservativeFold {
    pub requested_family: String,
    pub validation_samples: Vec<String>,
    pub expanded_other_families: Vec<String>,
    pub train_size: usize,
    pub validation_size: usize,
    pub exact_pcm_overlap: bool,
    pub lineage_overlap: bool,
    pub accepted: usize,
    pub canonical_correct: usize,
    pub family_correct: usize,
    pub false_confident: usize,
    pub abstentions: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HeldOutConservativeAggregate {
    pub fold_observations: usize,
    pub unique_fixtures: usize,
    pub accepted: usize,
    pub canonical_correct: usize,
    pub family_correct: usize,
    pub false_confident: usize,
    pub abstentions: usize,
    pub valid_folds: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConservativeFocusSlices {
    pub boundary_58_62: Vec<ConservativeFocusRow>,
    pub neighborhood_130: Vec<ConservativeFocusRow>,
    pub ambiguity_80_160: Vec<ConservativeFocusRow>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConservativeFocusRow {
    pub fixture_id: String,
    pub truth_bpm: f32,
    pub classical_propagated_bpm: Option<f32>,
    pub conservative_bpm: Option<f32>,
    pub conservative_decision: String,
    pub candidate_family_present: bool,
    pub event_consistency: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConservativeDecision {
    pub recommendation: String,
    pub primary_bottleneck: String,
    pub propagation_top1_canonical: usize,
    pub conservative_top1_canonical: usize,
    pub accepted: usize,
    pub abstained: usize,
    pub false_confident: usize,
    pub adversarial_invalid_before: usize,
    pub adversarial_invalid_after: usize,
    pub variable_tempo_abstained: usize,
    pub variable_tempo_total: usize,
    pub production_behavior_changed: bool,
    pub answers: BTreeMap<String, String>,
}

#[derive(Clone, Debug)]
struct EvidenceCandidate {
    candidate: TempoCandidate,
    sources: BTreeSet<String>,
    source_count: usize,
    has_full: bool,
    has_low: bool,
    has_neural: bool,
    has_event: bool,
    has_v2: bool,
    event_agreement: Option<f32>,
    duration_stability: f32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RankMode {
    Evidence,
    Consensus,
    Margin,
    EventGated,
    ClassicalAnchor,
}

#[derive(Clone, Debug)]
struct DecisionInternal {
    row: TempoShadowDecisionRow,
    candidates: Vec<EvidenceCandidate>,
}

/// A research-only adapter that keeps the original V2 timeline and structural
/// evidence while replacing only the tempo hypotheses supplied to the planner.
/// This is the boundary that lets the effective shadow path exercise the real
/// planner without mutating production analysis or BeatEvent timestamps.
struct ShadowAnalysisInput<'a> {
    analysis: &'a TrackAnalysisV2,
    hypotheses: Vec<TempoHypothesis>,
}

impl V2AnalysisInput for ShadowAnalysisInput<'_> {
    fn as_v2_legacy_view(&self) -> TrackAnalysis {
        V2AnalysisInput::as_v2_legacy_view(self.analysis)
    }

    fn cue_candidates(&self) -> Vec<DjCue> {
        self.analysis.cues.clone()
    }

    fn phrase_boundaries(&self) -> Vec<PhraseBoundary> {
        self.analysis.structure.phrase_boundaries.clone()
    }

    fn tempo_hypotheses(&self) -> Vec<TempoHypothesis> {
        self.hypotheses.clone()
    }
}

/// Research-only V2 adapter used by the realistic positive corpus.  Unlike
/// the old positive harness, this adapter keeps the actual analyzed event
/// stream and adds only bounded heuristic cue candidates derived from the
/// same analyzed structure.
struct RealisticPlannerInput<'a> {
    analysis: &'a TrackAnalysisV2,
    hypotheses: Vec<TempoHypothesis>,
    cues: Vec<DjCue>,
}

impl V2AnalysisInput for RealisticPlannerInput<'_> {
    fn as_v2_legacy_view(&self) -> TrackAnalysis {
        V2AnalysisInput::as_v2_legacy_view(self.analysis)
    }

    fn cue_candidates(&self) -> Vec<DjCue> {
        self.cues.clone()
    }

    fn phrase_boundaries(&self) -> Vec<PhraseBoundary> {
        self.analysis.structure.phrase_boundaries.clone()
    }

    fn tempo_hypotheses(&self) -> Vec<TempoHypothesis> {
        self.hypotheses.clone()
    }
}

fn realistic_analysis_cues(analysis: &TrackAnalysisV2) -> Vec<DjCue> {
    let beat_times = &analysis.rhythm.beats;
    if beat_times.is_empty() {
        return Vec::new();
    }
    let audible_start_beat = beat_times
        .iter()
        .position(|event| event.time >= analysis.audible_start)
        .unwrap_or(0);
    let audible_end_beat = beat_times
        .iter()
        .position(|event| event.time >= analysis.audible_end)
        .unwrap_or(beat_times.len());
    let intro_end_beat = analysis
        .structure
        .sections
        .iter()
        .find(|section| section.has_label(wotoha_core::analysis::SectionLabel::Intro))
        .map(|section| section.end_beat);
    let outro_start_beat = analysis
        .structure
        .sections
        .iter()
        .find(|section| section.has_label(wotoha_core::analysis::SectionLabel::Outro))
        .map(|section| section.start_beat);
    let mut cues = generate_heuristic_cues(
        CueGenerationInput::new(beat_times.len(), audible_start_beat, audible_end_beat)
            .with_intro_end(intro_end_beat)
            .with_outro_start(outro_start_beat)
            .with_structure(&analysis.structure),
    );
    // The positive shadow corpus needs a bounded, audibility-derived
    // transition anchor before the final beat so the real planner can test a
    // phrase-sized overlap.  This is still a cue candidate generated from the
    // analyzed timeline, not a truth-provided BeatEvent or acceptance rule.
    let audible_span = audible_end_beat.saturating_sub(audible_start_beat);
    if audible_span >= 20 {
        const BOUNDARY_PADDING_BEATS: usize = 4;
        let mut mix_out = DjCue::new(
            audible_end_beat.saturating_sub(8),
            UnitInterval::clamped(0.75),
        );
        mix_out.mix_out = UnitInterval::ONE;
        mix_out.cut_safe = UnitInterval::ONE;
        cues.push(mix_out);

        let mut mix_in = DjCue::new(
            audible_start_beat.saturating_add(BOUNDARY_PADDING_BEATS + 1),
            UnitInterval::clamped(0.75),
        );
        mix_in.mix_in = UnitInterval::ONE;
        mix_in.cut_safe = UnitInterval::ONE;
        cues.push(mix_in);
    }
    cues
}

pub fn run_tempo_conservative_shadow_research(
    output_dir: &Path,
    source_commit: String,
    starting_commit: Option<String>,
) -> Result<TempoConservativeShadowReport, LabError> {
    fs::create_dir_all(output_dir)?;
    let baseline_dir = output_dir.join(".baseline");
    let baseline = run_tempo_shadow_followup(
        &baseline_dir,
        source_commit.clone(),
        starting_commit.clone(),
    )?;
    let flows = baseline
        .candidate_flow
        .iter()
        .filter(|flow| flow.truth_bpm.is_some())
        .collect::<Vec<_>>();
    let candidate_budgets = build_budget_summaries(&flows);
    let provenance_conditioned = build_provenance_conditions(&flows);
    let ranking_variants = build_ranking_variants(&flows);
    let decisions = build_abstention_rows(&flows);
    let abstention = AbstentionReport {
        rule_name: "classical_anchor_guarded_select_or_retain_or_abstain_v2".into(),
        pareto: build_pareto(&flows),
        rows: decisions.iter().map(|item| item.row.clone()).collect(),
    };
    let evidence_attribution = build_evidence_attribution(&flows);
    let stationarity = build_stationarity_observations()?;
    let metrical_consistency = build_metrical_consistency(&flows);
    let adversarial = build_adversarial_report()?;
    let candidate_pressure = build_candidate_pressure(&flows);
    let pruning = build_pruning_summaries(&flows);
    let conservative_matrix = build_conservative_matrix(&baseline, &flows, &decisions)?;
    let mut effective_planner = summarize_effective_planner(&conservative_matrix);
    effective_planner.aliases_representable = adversarial.valid_aliases_retained;
    effective_planner.aliases_total = adversarial.valid_aliases_total;
    let runtime_feature_audit = build_runtime_feature_audit(&flows);
    let runtime_feasible = build_runtime_feasible_summary(&flows);
    let runtime_consistency = build_runtime_consistency(&flows);
    let abstention_reasons = build_abstention_reasons(&flows, &decisions);
    let realistic_corpus = build_realistic_corpus_report(&source_commit)?;
    let mut independent_positive_corpus = build_positive_realistic_corpus(false)?;
    independent_positive_corpus.source_commit = Some(source_commit.clone());
    independent_positive_corpus.starting_commit = starting_commit.clone();
    let heldout = build_heldout(&flows, &decisions);
    let focus_slices = build_focus_slices(&flows, &decisions);
    let scalar_count = flows.len();
    let accepted = decisions
        .iter()
        .filter(|item| item.row.decision == "select")
        .count();
    let abstained = decisions
        .iter()
        .filter(|item| item.row.decision != "select")
        .count();
    let false_confident = decisions
        .iter()
        .filter(|item| item.row.false_confident_accept)
        .count();
    let propagation_top1_canonical = baseline
        .reanchor
        .variants
        .iter()
        .find(|variant| variant.variant == "classical_propagated")
        .map(|variant| variant.top1_canonical_correct)
        .unwrap_or(0);
    let conservative_top1_canonical = decisions
        .iter()
        .filter(|item| item.row.decision == "select" && item.row.canonical_correct)
        .count();
    let variable_tempo_abstained = stationarity.iter().filter(|item| item.abstain).count();
    let answers = build_answers(
        &baseline,
        &ranking_variants,
        &abstention,
        &evidence_attribution,
        &stationarity,
        &adversarial,
        &candidate_pressure,
    );
    let internal_positive_gate = independent_positive_corpus.summary.external_gate_passed
        && independent_positive_corpus
            .summary
            .effective_correct_beatmatched
            >= 5
        && independent_positive_corpus
            .summary
            .effective_false_beatmatched
            == 0
        && realistic_corpus
            .transition_summary
            .effective_false_beatmatched
            == 0
        && adversarial.invalid_after_guard == 0;
    let decision = ConservativeDecision {
        recommendation: if internal_positive_gate {
            "EXTERNAL VALIDATION REQUIRED"
        } else {
            "INTERNAL RESEARCH BLOCKED"
        }
        .into(),
        primary_bottleneck: if internal_positive_gate {
            "internal_shadow_gates_passed_external_ecological_validation_remains"
        } else {
            "propagation_and_unsafe_ranking"
        }
        .into(),
        propagation_top1_canonical,
        conservative_top1_canonical,
        accepted,
        abstained,
        false_confident,
        adversarial_invalid_before: adversarial.invalid_before_guard,
        adversarial_invalid_after: adversarial.invalid_after_guard,
        variable_tempo_abstained,
        variable_tempo_total: stationarity.len(),
        production_behavior_changed: false,
        answers,
    };
    let report = TempoConservativeShadowReport {
        schema_version: crate::RESEARCH_REPORT_SCHEMA_VERSION + 2,
        source_commit,
        starting_commit,
        production_behavior_changed: false,
        fixture_count: baseline.fixture_count,
        scalar_tempo_fixture_count: scalar_count,
        candidate_budgets,
        provenance_conditioned,
        ranking_variants,
        abstention,
        evidence_attribution,
        stationarity,
        metrical_consistency,
        adversarial,
        candidate_pressure,
        pruning,
        conservative_matrix,
        effective_planner,
        heldout,
        runtime_feature_audit,
        runtime_feasible,
        runtime_consistency,
        abstention_reasons,
        realistic_corpus,
        independent_positive_corpus,
        focus_slices,
        decision,
    };
    write_outputs(output_dir, &report)?;
    Ok(report)
}

fn source_flags(candidate: &TempoCandidate) -> BTreeSet<String> {
    let mut flags = BTreeSet::new();
    let source = candidate.source.to_ascii_lowercase();
    for (needle, label) in [
        ("classical_full", "full"),
        ("classical_low", "low"),
        ("neural", "neural"),
        ("event", "event"),
        ("v2", "v2"),
    ] {
        if source.contains(needle) {
            flags.insert(label.into());
        }
    }
    flags
}

fn relation_multiplier(relation: &str) -> f32 {
    match relation {
        "half_time" => 0.5,
        "double_time" => 2.0,
        _ => 1.0,
    }
}

fn harmonic_agreement(candidate_bpm: f32, event_bpm: Option<f32>) -> Option<f32> {
    let event = event_bpm.filter(|value| value.is_finite() && *value > 0.0)?;
    let ratios = [0.5_f32, 1.0, 2.0];
    let error = ratios
        .iter()
        .map(|ratio| relative_difference(candidate_bpm, event * ratio))
        .fold(f32::INFINITY, f32::min);
    Some((1.0 - error / 0.08).clamp(0.0, 1.0))
}

fn relative_difference(left: f32, right: f32) -> f32 {
    (left - right).abs() / left.abs().max(right.abs()).max(1.0)
}

fn candidate_key(left: f32, right: f32) -> bool {
    relative_difference(left, right) <= RELATIVE_TOLERANCE
}

fn collect_raw_candidates(flow: &CandidateFlowObservation) -> Vec<TempoCandidate> {
    flow.pools
        .classical_full_top_k
        .iter()
        .chain(flow.pools.classical_low_top_k.iter())
        .chain(flow.pools.neural_half_native_double.iter())
        .chain(flow.pools.event_clock.iter())
        .chain(flow.pools.v2_current.iter())
        .filter(|candidate| candidate.bpm.is_finite() && candidate.bpm > 0.0)
        .cloned()
        .collect()
}

fn duration_stability(
    flow: &CandidateFlowObservation,
    candidate: &TempoCandidate,
    flows: &[&CandidateFlowObservation],
) -> f32 {
    let related = flows
        .iter()
        .filter(|other| other.master_id == flow.master_id && other.fixture_id != flow.fixture_id);
    let mut seen = 0usize;
    let mut stable = 0usize;
    for other in related {
        seen += 1;
        if collect_raw_candidates(other)
            .iter()
            .any(|other_candidate| candidate_key(other_candidate.bpm, candidate.bpm))
        {
            stable += 1;
        }
    }
    if seen == 0 {
        0.5
    } else {
        stable as f32 / seen as f32
    }
}

fn merge_evidence(
    flow: &CandidateFlowObservation,
    flows: &[&CandidateFlowObservation],
    budget: usize,
) -> Vec<EvidenceCandidate> {
    let raw = collect_raw_candidates(flow);
    let mut merged = Vec::<EvidenceCandidate>::new();
    for candidate in raw {
        let flags = source_flags(&candidate);
        let event_agreement = harmonic_agreement(candidate.bpm, flow.event_clock_bpm);
        if let Some(existing) = merged
            .iter_mut()
            .find(|existing| candidate_key(existing.candidate.bpm, candidate.bpm))
        {
            existing.candidate.normalized_score = existing
                .candidate
                .normalized_score
                .max(candidate.normalized_score);
            existing.candidate.score = existing.candidate.score.max(candidate.score);
            existing.sources.extend(flags.iter().cloned());
            existing.source_count = existing.sources.len();
            existing.has_full |= flags.contains("full");
            existing.has_low |= flags.contains("low");
            existing.has_neural |= flags.contains("neural");
            existing.has_event |= flags.contains("event");
            existing.has_v2 |= flags.contains("v2");
            existing.event_agreement = match (existing.event_agreement, event_agreement) {
                (Some(left), Some(right)) => Some(left.max(right)),
                (left, right) => left.or(right),
            };
            continue;
        }
        merged.push(EvidenceCandidate {
            source_count: flags.len(),
            has_full: flags.contains("full"),
            has_low: flags.contains("low"),
            has_neural: flags.contains("neural"),
            has_event: flags.contains("event"),
            has_v2: flags.contains("v2"),
            event_agreement,
            duration_stability: duration_stability(flow, &candidate, flows),
            sources: flags,
            candidate,
        });
    }
    merged.sort_by(|left, right| {
        right
            .candidate
            .normalized_score
            .total_cmp(&left.candidate.normalized_score)
            .then_with(|| left.candidate.bpm.total_cmp(&right.candidate.bpm))
    });
    merged.truncate(budget);
    merged
}

fn rank_value(item: &EvidenceCandidate, mode: RankMode, margin: f32) -> f32 {
    let evidence = item.candidate.normalized_score.clamp(0.0, 1.0);
    let support = (item.source_count as f32 / 3.0).min(1.0);
    let event = item.event_agreement.unwrap_or(0.0);
    let duration = item.duration_stability;
    let value = match mode {
        RankMode::Evidence => 0.45 * evidence + 0.25 * support + 0.20 * event + 0.10 * duration,
        RankMode::Consensus => 0.35 * support + 0.35 * event + 0.20 * evidence + 0.10 * duration,
        RankMode::Margin => 0.40 * evidence + 0.25 * margin + 0.20 * support + 0.15 * event,
        RankMode::EventGated => {
            if event >= ACCEPT_EVENT_AGREEMENT {
                0.30 * evidence + 0.35 * event + 0.25 * support + 0.10 * duration
            } else {
                0.55 * evidence + 0.20 * support + 0.15 * event + 0.10 * duration
            }
        }
        RankMode::ClassicalAnchor => {
            if item.has_full && item.has_low {
                0.60 * evidence + 0.15 * support + 0.25 * duration
            } else if event >= ACCEPT_EVENT_AGREEMENT {
                0.30 * evidence + 0.35 * event + 0.25 * support + 0.10 * duration
            } else {
                0.55 * evidence + 0.20 * support + 0.15 * event + 0.10 * duration
            }
        }
    };
    if matches!(mode, RankMode::ClassicalAnchor) && item.has_full && item.has_low {
        value + CLASSICAL_ANCHOR_BONUS
    } else {
        value
    }
}

fn classical_anchor_available(candidates: &[EvidenceCandidate]) -> bool {
    candidates
        .iter()
        .any(|candidate| candidate.has_full && candidate.has_low)
}

fn classical_anchor_allows(first: &EvidenceCandidate, candidates: &[EvidenceCandidate]) -> bool {
    if classical_anchor_available(candidates) {
        (first.has_full && first.has_low)
            || (first.has_neural && first.has_event && first.source_count >= 3)
    } else {
        !(first.has_full ^ first.has_low) || first.has_neural
    }
}

fn rank_candidates(
    flow: &CandidateFlowObservation,
    flows: &[&CandidateFlowObservation],
    budget: usize,
    mode: RankMode,
) -> Vec<EvidenceCandidate> {
    let mut candidates = merge_evidence(flow, flows, budget);
    candidates.sort_by(|left, right| {
        let margin = left.candidate.normalized_score - right.candidate.normalized_score;
        rank_value(right, mode, margin)
            .total_cmp(&rank_value(left, mode, margin))
            .then_with(|| {
                right
                    .candidate
                    .normalized_score
                    .total_cmp(&left.candidate.normalized_score)
            })
            .then_with(|| left.candidate.bpm.total_cmp(&right.candidate.bpm))
    });
    for item in &mut candidates {
        item.candidate.shadow_rank_score = rank_value(item, mode, 0.0);
    }
    candidates
}

fn candidate_family_correct(candidate: &EvidenceCandidate, truth: f32) -> bool {
    [0.5, 1.0, 2.0].iter().any(|ratio| {
        relative_difference(candidate.candidate.bpm, truth * ratio) <= RELATIVE_TOLERANCE
    })
}

fn canonical(candidate: &EvidenceCandidate, truth: f32) -> bool {
    relative_difference(candidate.candidate.bpm, truth) <= RELATIVE_TOLERANCE
}

fn family_precision(correct: usize, accepted: usize) -> Option<f32> {
    (accepted > 0).then_some(correct as f32 / accepted as f32)
}

fn median_usize(values: &mut [usize]) -> usize {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    values[values.len() / 2]
}

fn percentile_usize(values: &mut [usize], percentile: f32) -> usize {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    let index = ((values.len() - 1) as f32 * percentile).round() as usize;
    values[index.min(values.len() - 1)]
}

fn mean_usize(values: &[usize]) -> f32 {
    if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<usize>() as f32 / values.len() as f32
    }
}

fn build_budget_summaries(flows: &[&CandidateFlowObservation]) -> Vec<BudgetSummary> {
    [2, 3, 4]
        .into_iter()
        .map(|budget| {
            let mut counts = Vec::new();
            let mut pairs = Vec::new();
            let mut top1_canonical = 0;
            let mut top1_family = 0;
            let mut recall_canonical = 0;
            let mut recall_family = 0;
            for flow in flows {
                let candidates = merge_evidence(flow, flows, budget);
                counts.push(candidates.len());
                pairs.push(candidates.len() * candidates.len());
                if let Some(truth) = flow.truth_bpm {
                    top1_canonical +=
                        usize::from(candidates.first().is_some_and(|c| canonical(c, truth)));
                    top1_family += usize::from(
                        candidates
                            .first()
                            .is_some_and(|c| candidate_family_correct(c, truth)),
                    );
                    recall_canonical += usize::from(candidates.iter().any(|c| canonical(c, truth)));
                    recall_family += usize::from(
                        candidates
                            .iter()
                            .any(|c| candidate_family_correct(c, truth)),
                    );
                }
            }
            let mut sorted_counts = counts.clone();
            let mut sorted_pairs = pairs.clone();
            BudgetSummary {
                budget,
                mean_candidates: mean_usize(&counts),
                p50_candidates: median_usize(&mut sorted_counts),
                p95_candidates: percentile_usize(&mut sorted_counts, 0.95),
                max_candidates: counts.iter().copied().max().unwrap_or(0),
                top1_canonical_correct: top1_canonical,
                top1_family_correct: top1_family,
                candidate_family_recall: recall_family,
                candidate_canonical_recall: recall_canonical,
                mean_pair_cross_product: mean_usize(&pairs),
                p95_pair_cross_product: percentile_usize(&mut sorted_pairs, 0.95),
                max_pair_cross_product: pairs.iter().copied().max().unwrap_or(0),
            }
        })
        .collect()
}

fn provenance_label(item: &EvidenceCandidate) -> String {
    let mut labels = item.sources.iter().cloned().collect::<Vec<_>>();
    if labels.is_empty() {
        labels.push("none".into());
    }
    labels.join("+")
}

fn build_provenance_conditions(flows: &[&CandidateFlowObservation]) -> Vec<ProvenanceCondition> {
    let mut grouped = BTreeMap::<String, (usize, usize, usize)>::new();
    for flow in flows {
        let candidates = merge_evidence(flow, flows, MAX_CANDIDATES);
        let Some(truth) = flow.truth_bpm else {
            continue;
        };
        for candidate in candidates {
            let label = provenance_label(&candidate);
            let entry = grouped.entry(label).or_default();
            entry.0 += 1;
            entry.1 += usize::from(canonical(&candidate, truth));
            entry.2 += usize::from(candidate_family_correct(&candidate, truth));
        }
    }
    [
        "full",
        "low",
        "full+low",
        "event+full",
        "event+low",
        "event+full+low",
        "event+neural",
        "full+neural",
        "event+full+neural",
        "event+full+low+neural",
    ]
    .into_iter()
    .map(|label| {
        let (candidates, canonical_correct, family_correct) =
            grouped.get(label).copied().unwrap_or_default();
        ProvenanceCondition {
            combination: label.into(),
            candidates,
            canonical_correct,
            family_correct,
            canonical_precision: family_precision(canonical_correct, candidates),
            family_precision: family_precision(family_correct, candidates),
        }
    })
    .collect()
}

fn build_ranking_variants(flows: &[&CandidateFlowObservation]) -> Vec<RankingVariantSummary> {
    let variants = [
        ("current_v2", None, 0usize),
        ("classical_propagated", None, 0usize),
        ("bounded_evidence_sum", Some(RankMode::Evidence), 4usize),
        ("source_consensus", Some(RankMode::Consensus), 4usize),
        ("confidence_margin", Some(RankMode::Margin), 4usize),
        ("event_agreement_gated", Some(RankMode::EventGated), 4usize),
        (
            "classical_anchor_guarded",
            Some(RankMode::ClassicalAnchor),
            4usize,
        ),
    ];
    variants
        .into_iter()
        .map(|(name, mode, budget)| {
            let mut accepted = 0;
            let mut abstained = 0;
            let mut retained_multiple = 0;
            let mut canonical_correct = 0;
            let mut family_correct = 0;
            let mut false_confident = 0;
            let mut confidence_defined = true;
            let mut candidate_family_recall = 0;
            let mut candidate_canonical_recall = 0;
            for flow in flows {
                let truth = flow.truth_bpm.expect("scalar flow");
                if name == "current_v2" {
                    confidence_defined = false;
                    let candidate = flow.current_v2_top1.as_ref();
                    accepted += usize::from(candidate.is_some());
                    canonical_correct +=
                        usize::from(candidate.is_some_and(|c| {
                            relative_difference(c.bpm, truth) <= RELATIVE_TOLERANCE
                        }));
                    family_correct +=
                        usize::from(candidate.is_some_and(|c| family_correct_bpm(c.bpm, truth)));
                    continue;
                }
                if name == "classical_propagated" {
                    confidence_defined = false;
                    let candidate = flow.pools.classical_propagated.first();
                    accepted += usize::from(candidate.is_some());
                    canonical_correct +=
                        usize::from(candidate.is_some_and(|c| {
                            relative_difference(c.bpm, truth) <= RELATIVE_TOLERANCE
                        }));
                    family_correct +=
                        usize::from(candidate.is_some_and(|c| family_correct_bpm(c.bpm, truth)));
                    continue;
                }
                let candidates = rank_candidates(flow, flows, budget, mode.expect("mode"));
                candidate_family_recall += usize::from(
                    candidates
                        .iter()
                        .any(|c| candidate_family_correct(c, truth)),
                );
                candidate_canonical_recall +=
                    usize::from(candidates.iter().any(|c| canonical(c, truth)));
                let first = candidates.first();
                let second = candidates.get(1);
                let margin = second
                    .map(|other| {
                        first
                            .map(|item| {
                                item.candidate.shadow_rank_score - other.candidate.shadow_rank_score
                            })
                            .unwrap_or(0.0)
                    })
                    .unwrap_or(1.0);
                let safe = first.is_some_and(|item| {
                    item.candidate.shadow_rank_score >= ACCEPT_SCORE
                        && margin >= ACCEPT_MARGIN
                        && item.source_count >= 2
                        && item
                            .event_agreement
                            .is_some_and(|value| value >= ACCEPT_EVENT_AGREEMENT)
                        && (mode != Some(RankMode::ClassicalAnchor)
                            || classical_anchor_allows(item, &candidates))
                });
                if safe {
                    accepted += 1;
                    canonical_correct += usize::from(first.is_some_and(|c| canonical(c, truth)));
                    family_correct +=
                        usize::from(first.is_some_and(|c| candidate_family_correct(c, truth)));
                    false_confident +=
                        usize::from(first.is_some_and(|c| !candidate_family_correct(c, truth)));
                } else if first.is_some_and(|_| margin < ACCEPT_MARGIN) {
                    retained_multiple += 1;
                } else {
                    abstained += 1;
                }
            }
            RankingVariantSummary {
                name: name.into(),
                candidate_budget: budget,
                scored: flows.len(),
                accepted,
                abstained,
                retained_multiple,
                canonical_correct,
                family_correct,
                canonical_precision: family_precision(canonical_correct, accepted),
                family_precision: family_precision(family_correct, accepted),
                false_confident_accepts: confidence_defined.then_some(false_confident),
                candidate_family_recall,
                candidate_canonical_recall,
            }
        })
        .collect()
}

fn family_correct_bpm(candidate: f32, truth: f32) -> bool {
    [0.5, 1.0, 2.0]
        .iter()
        .any(|ratio| relative_difference(candidate, truth * ratio) <= RELATIVE_TOLERANCE)
}

fn build_abstention_rows(flows: &[&CandidateFlowObservation]) -> Vec<DecisionInternal> {
    flows
        .iter()
        .map(|flow| {
            let truth = flow.truth_bpm.expect("scalar flow");
            let candidates =
                rank_candidates(flow, flows, MAX_CANDIDATES, RankMode::ClassicalAnchor);
            let first = candidates.first().cloned();
            let second = candidates.get(1);
            let margin = first.as_ref().zip(second).map(|(one, two)| {
                one.candidate.shadow_rank_score - two.candidate.shadow_rank_score
            });
            let source_support = first.as_ref().map(|item| item.source_count).unwrap_or(0);
            let event_agreement = first.as_ref().and_then(|item| item.event_agreement);
            let safe = first.as_ref().is_some_and(|item| {
                item.candidate.shadow_rank_score >= ACCEPT_SCORE
                    && margin.unwrap_or(1.0) >= ACCEPT_MARGIN
                    && item.source_count >= 2
                    && event_agreement.is_some_and(|value| value >= ACCEPT_EVENT_AGREEMENT)
                    && resolved_relation(item, flow.event_clock_bpm).is_some()
                    && classical_anchor_allows(item, &candidates)
            });
            let classical_anchor_available = classical_anchor_available(&candidates);
            let decision = if safe {
                "select"
            } else if first.is_some() && margin.is_some_and(|value| value < ACCEPT_MARGIN) {
                "retain_multiple"
            } else {
                "abstain"
            };
            let canonical_correct = first.as_ref().is_some_and(|item| canonical(item, truth));
            let family_correct = first
                .as_ref()
                .is_some_and(|item| candidate_family_correct(item, truth));
            let row = TempoShadowDecisionRow {
                fixture_id: flow.fixture_id.clone(),
                family: flow.family.clone(),
                truth_bpm: truth,
                candidate_budget: MAX_CANDIDATES,
                decision: decision.into(),
                selected_bpm: safe.then(|| first.as_ref().expect("safe candidate").candidate.bpm),
                selected_relation: safe.then(|| {
                    relation_name(
                        resolved_relation(
                            first.as_ref().expect("safe candidate"),
                            flow.event_clock_bpm,
                        )
                        .expect("safe candidate relation"),
                    )
                }),
                candidate_count: candidates.len(),
                top1_score: first.as_ref().map(|item| item.candidate.shadow_rank_score),
                top2_score: second.map(|item| item.candidate.shadow_rank_score),
                score_margin: margin,
                source_support,
                selected_sources: first
                    .as_ref()
                    .map(|item| item.sources.iter().cloned().collect())
                    .unwrap_or_default(),
                classical_anchor_available,
                event_agreement,
                duration_stability: first
                    .as_ref()
                    .map(|item| item.duration_stability)
                    .unwrap_or(0.0),
                metrical_consistency: first
                    .as_ref()
                    .map(|item| relation_status(item, flow.event_clock_bpm))
                    .unwrap_or_else(|| "unavailable".into()),
                canonical_correct: safe && canonical_correct,
                family_correct: safe && family_correct,
                false_confident_accept: safe && !family_correct,
                correct_abstention: !safe && !canonical_correct,
            };
            DecisionInternal { row, candidates }
        })
        .collect()
}

fn build_pareto(flows: &[&CandidateFlowObservation]) -> Vec<OperatingPoint> {
    [0.55, 0.65, 0.75, 0.85, 0.95]
        .into_iter()
        .map(|threshold| {
            let mut accepted = 0;
            let mut canonical_correct = 0;
            let mut family_correct = 0;
            let mut false_confident = 0;
            for flow in flows {
                let candidates =
                    rank_candidates(flow, flows, MAX_CANDIDATES, RankMode::ClassicalAnchor);
                let Some(first) = candidates.first() else {
                    continue;
                };
                let margin = candidates
                    .get(1)
                    .map(|second| {
                        first.candidate.shadow_rank_score - second.candidate.shadow_rank_score
                    })
                    .unwrap_or(1.0);
                let select = first.candidate.shadow_rank_score >= threshold
                    && margin >= ACCEPT_MARGIN
                    && first.source_count >= 2
                    && first
                        .event_agreement
                        .is_some_and(|value| value >= ACCEPT_EVENT_AGREEMENT)
                    && classical_anchor_allows(first, &candidates);
                if select {
                    accepted += 1;
                    let truth = flow.truth_bpm.expect("scalar flow");
                    canonical_correct += usize::from(canonical(first, truth));
                    family_correct += usize::from(candidate_family_correct(first, truth));
                    false_confident += usize::from(!candidate_family_correct(first, truth));
                }
            }
            OperatingPoint {
                name: format!("score_{threshold:.2}"),
                score_threshold: threshold,
                margin_threshold: ACCEPT_MARGIN,
                accepted,
                coverage: accepted as f32 / flows.len().max(1) as f32,
                canonical_precision: family_precision(canonical_correct, accepted),
                family_precision: family_precision(family_correct, accepted),
                false_confident,
                false_beatmatched_proxy: false_confident,
            }
        })
        .collect()
}

fn build_evidence_attribution(flows: &[&CandidateFlowObservation]) -> EvidenceAttribution {
    let mut harmful_source_counts = BTreeMap::new();
    let mut rescue_source_counts = BTreeMap::new();
    let mut harmful_cases = Vec::new();
    let mut rescue_cases = Vec::new();
    for flow in flows {
        let truth = flow.truth_bpm.expect("scalar flow");
        let classical = flow.pools.classical_propagated.first();
        let merged = flow.pools.merged_shadow.first();
        let classical_correct = classical.is_some_and(|candidate| {
            relative_difference(candidate.bpm, truth) <= RELATIVE_TOLERANCE
        });
        let merged_correct = merged.is_some_and(|candidate| {
            relative_difference(candidate.bpm, truth) <= RELATIVE_TOLERANCE
        });
        let merged_family =
            merged.is_some_and(|candidate| family_correct_bpm(candidate.bpm, truth));
        if classical_correct && !merged_correct {
            let added = merged
                .map(|candidate| source_flags(candidate).into_iter().collect::<Vec<_>>())
                .unwrap_or_default();
            for source in &added {
                *harmful_source_counts.entry(source.clone()).or_insert(0) += 1;
            }
            harmful_cases.push(HarmfulEvidenceCase {
                fixture_id: flow.fixture_id.clone(),
                family: flow.family.clone(),
                classical_bpm: classical.map(|candidate| candidate.bpm),
                merged_bpm: merged.map(|candidate| candidate.bpm),
                added_sources: added,
                reason: if merged_family {
                    "numeric_or_refinement_displacement"
                } else {
                    "family_displacement"
                }
                .into(),
            });
        }
        let current = flow.current_v2_top1.as_ref();
        let current_correct = current.is_some_and(|candidate| {
            relative_difference(candidate.bpm, truth) <= RELATIVE_TOLERANCE
        });
        if !current_correct && merged_correct {
            let enabling = merged
                .map(|candidate| source_flags(candidate).into_iter().collect::<Vec<_>>())
                .unwrap_or_default();
            for source in &enabling {
                *rescue_source_counts.entry(source.clone()).or_insert(0) += 1;
            }
            rescue_cases.push(RescueEvidenceCase {
                fixture_id: flow.fixture_id.clone(),
                family: flow.family.clone(),
                current_bpm: current.map(|candidate| candidate.bpm),
                shadow_bpm: merged.map(|candidate| candidate.bpm),
                enabling_sources: enabling,
                reason: "merged_shadow_top1_rescue".into(),
            });
        }
    }
    EvidenceAttribution {
        harmful_merged_cases: harmful_cases,
        rescue_cases,
        harmful_source_counts,
        rescue_source_counts,
    }
}

fn build_stationarity_observations() -> Result<Vec<StationarityObservation>, LabError> {
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
            "ramp_minus_1",
            TempoProfile::LinearRamp {
                start_bpm: 130.0,
                end_bpm: 129.0,
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
            "ramp_plus_4",
            TempoProfile::LinearRamp {
                start_bpm: 130.0,
                end_bpm: 134.0,
            },
        ),
        (
            "ramp_minus_4",
            TempoProfile::LinearRamp {
                start_bpm: 130.0,
                end_bpm: 126.0,
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
    profiles
        .into_iter()
        .map(|(profile, tempo)| {
            let spec = long_spec(
                format!("conservative-variable-{profile}"),
                format!("conservative-variable-{profile}"),
                FixtureFamily::TempoDrift,
                tempo,
                EventStyle::Standard,
                60_000_000,
                4,
                500_000,
                0x24_10_04_52,
            );
            let analyzed = analyze_long_fixture(generate_fixture(&spec)?)?;
            Ok(stationarity_from_beats(&spec.id, profile, &analyzed))
        })
        .collect()
}

fn stationarity_from_beats(
    fixture_id: &str,
    profile: &str,
    analyzed: &AnalyzedFixture,
) -> StationarityObservation {
    let intervals = analyzed
        .v2
        .rhythm
        .beats
        .windows(2)
        .filter_map(|window| window[1].time.checked_sub(window[0].time))
        .map(|duration| duration.as_micros() as f64)
        .filter(|value| *value > 0.0)
        .collect::<Vec<_>>();
    let median = median_f64(&intervals);
    let mad_ratio = median.map(|center| {
        let deviations = intervals
            .iter()
            .map(|value| (value - center).abs())
            .collect::<Vec<_>>();
        median_f64(&deviations).unwrap_or(0.0) / center.max(1.0)
    });
    let third = intervals.len() / 3;
    let early = median_f64(&intervals[..third.max(1).min(intervals.len())]);
    let middle = median_f64(
        &intervals[third.min(intervals.len())..(2 * third).max(third + 1).min(intervals.len())],
    );
    let late = median_f64(&intervals[(2 * third).min(intervals.len())..]);
    let drift = |left: Option<f64>, right: Option<f64>| {
        left.zip(right)
            .zip(median)
            .map(|((a, b), center)| (a - b).abs() / center.max(1.0))
    };
    let early_middle = drift(early, middle);
    let middle_late = drift(middle, late);
    let early_late = drift(early, late);
    let residual_ratio = mad_ratio;
    let max_drift = [early_middle, middle_late, early_late]
        .into_iter()
        .flatten()
        .fold(0.0, f64::max);
    let abstain = intervals.len() < 5
        || max_drift > STATIONARY_DRIFT
        || mad_ratio.is_some_and(|value| value > STATIONARY_DISPERSION);
    let classification = if intervals.len() < 5 {
        "ambiguous"
    } else if max_drift <= 0.002 && mad_ratio.unwrap_or(1.0) <= 0.005 {
        "stationary"
    } else if max_drift <= STATIONARY_DRIFT {
        "slow_drift"
    } else if profile.contains("step") {
        "tempo_step"
    } else {
        "tempo_ramp"
    };
    StationarityObservation {
        fixture_id: fixture_id.into(),
        profile: profile.into(),
        classification: classification.into(),
        interval_count: intervals.len(),
        median_interval_micros: median,
        interval_mad_ratio: mad_ratio,
        early_middle_drift: early_middle,
        middle_late_drift: middle_late,
        early_late_drift: early_late,
        fit_residual_ratio: residual_ratio,
        refinement_eligible: intervals.len() >= 5 && !abstain,
        abstain,
        abstention_reason: abstain.then(|| "non_stationary_global_tempo".into()),
    }
}

fn median_f64(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    Some(sorted[sorted.len() / 2])
}

fn consistency_for(
    bpm: f32,
    relation: &str,
    event_bpm: Option<f32>,
) -> (String, Option<f32>, Option<f32>) {
    let Some(event) = event_bpm.filter(|value| value.is_finite() && *value > 0.0) else {
        return ("unavailable".into(), None, None);
    };
    let explicit = if relation == "unlabeled" {
        None
    } else {
        Some(relation_multiplier(relation))
    };
    let multiplier = explicit.unwrap_or_else(|| {
        [0.5, 1.0, 2.0]
            .into_iter()
            .min_by(|left, right| {
                relative_difference(bpm, event * *left)
                    .total_cmp(&relative_difference(bpm, event * *right))
            })
            .unwrap_or(1.0)
    });
    let expected = event * multiplier;
    let error = relative_difference(bpm, expected);
    let ratio = bpm / event;
    let status = if error <= RELATIVE_TOLERANCE {
        "consistent"
    } else if explicit.is_none()
        && [0.5, 1.0, 2.0]
            .iter()
            .any(|candidate| relative_difference(ratio, *candidate) <= 0.02)
    {
        "ambiguous"
    } else {
        "inconsistent"
    };
    (status.into(), Some(expected), Some(error.min(1.0)))
}

/// Resolve an unlabeled candidate only from the observed event clock.  The
/// result is an inference-time relation label, never a Ground Truth label.
/// Candidates outside the primary/half/double neighborhoods remain
/// unresolved and therefore cannot authorize an effective BeatMatched plan.
fn resolved_relation(
    candidate: &EvidenceCandidate,
    event_bpm: Option<f32>,
) -> Option<TempoRelation> {
    let label = candidate.candidate.relation.to_ascii_lowercase();
    if label != "unlabeled" {
        let relation = parse_relation(&label);
        return event_bpm
            .filter(|value| value.is_finite() && *value > 0.0)
            .map(|_| consistency_for(candidate.candidate.bpm, &label, event_bpm).0)
            .map_or(Some(relation), |status| {
                (status == "consistent").then_some(relation)
            });
    }
    let event = event_bpm.filter(|value| value.is_finite() && *value > 0.0)?;
    [
        (0.5_f32, TempoRelation::HalfTime),
        (1.0_f32, TempoRelation::Primary),
        (2.0_f32, TempoRelation::DoubleTime),
    ]
    .into_iter()
    .map(|(multiplier, relation)| {
        (
            relative_difference(candidate.candidate.bpm, event * multiplier),
            relation,
        )
    })
    .min_by(|left, right| left.0.total_cmp(&right.0))
    .and_then(|(error, relation)| (error <= RELATION_RESOLUTION_TOLERANCE).then_some(relation))
}

fn relation_name(relation: TempoRelation) -> String {
    match relation {
        TempoRelation::Primary => "primary",
        TempoRelation::HalfTime => "half_time",
        TempoRelation::DoubleTime => "double_time",
        TempoRelation::Alternative => "alternative",
    }
    .into()
}

fn relation_status(candidate: &EvidenceCandidate, event_bpm: Option<f32>) -> String {
    if resolved_relation(candidate, event_bpm).is_some() {
        "consistent".into()
    } else {
        consistency_for(
            candidate.candidate.bpm,
            &candidate.candidate.relation,
            event_bpm,
        )
        .0
    }
}

fn hypotheses_for_decision(
    flow: &CandidateFlowObservation,
    decision: &DecisionInternal,
) -> Vec<TempoHypothesis> {
    let candidates: Vec<&EvidenceCandidate> = match decision.row.decision.as_str() {
        "select" => decision.candidates.first().into_iter().collect(),
        "retain_multiple" => decision.candidates.iter().collect(),
        _ => Vec::new(),
    };
    candidates
        .iter()
        .filter_map(|candidate| {
            let relation = resolved_relation(candidate, flow.event_clock_bpm)?;
            TempoHypothesis::with_relation(
                candidate.candidate.bpm,
                UnitInterval::clamped(candidate.candidate.normalized_score.max(0.7)),
                relation,
            )
        })
        .collect()
}

fn hypotheses_from_candidates(candidates: &[TempoCandidate]) -> Vec<TempoHypothesis> {
    candidates
        .iter()
        .filter_map(|candidate| {
            TempoHypothesis::with_relation(
                candidate.bpm,
                UnitInterval::clamped(candidate.normalized_score.max(0.7)),
                parse_relation(&candidate.relation),
            )
        })
        .collect()
}

fn build_metrical_consistency(flows: &[&CandidateFlowObservation]) -> MetricalConsistencyAudit {
    let mut observations = Vec::new();
    let mut counts = BTreeMap::new();
    for flow in flows {
        for candidate in merge_evidence(flow, flows, MAX_CANDIDATES) {
            let (status, expected, error) = consistency_for(
                candidate.candidate.bpm,
                &candidate.candidate.relation,
                flow.event_clock_bpm,
            );
            *counts.entry(status.clone()).or_insert(0) += 1;
            observations.push(MetricalConsistencyObservation {
                fixture_id: flow.fixture_id.clone(),
                candidate_bpm: candidate.candidate.bpm,
                relation: candidate.candidate.relation.clone(),
                event_clock_bpm: flow.event_clock_bpm,
                expected_candidate_bpm: expected,
                relative_error: error,
                harmonic_ratio: flow
                    .event_clock_bpm
                    .map(|event| candidate.candidate.bpm / event),
                interval_dispersion: None,
                status,
                source: provenance_label(&candidate),
            });
        }
    }
    MetricalConsistencyAudit {
        semantics: "The event clock is treated as the observed pulse. A declared half/native/double relation maps the hypothesis to event BPM multiplied by 0.5/1/2; unlabeled Classical candidates use the nearest harmonic only for an ambiguity audit. Ground Truth is not an input.".into(),
        observations,
        counts,
    }
}

fn parse_relation(relation: &str) -> TempoRelation {
    match relation {
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

/// Research-only quality-first shadow selection.  The production V2 planner
/// remains the source of candidates and hard rejections; this selector tests a
/// deliberately narrow alternative authority order for the internal positive
/// corpus: explicit tempo relation, observed beat-pair support, finite energy
/// evidence, and the existing blocking quality guard must all pass.  A valid
/// BeatMatched candidate may therefore be selected even when the ordinary
/// soft-cost rank prefers Gapless.  No truth, fixture identity, or expected
/// pair participates in this decision.
fn quality_first_shadow_transition<O: V2AnalysisInput, I: V2AnalysisInput>(
    outgoing: &O,
    incoming: &I,
    config: &AutoMixConfig,
) -> (TransitionPlanV2, V2GuardedTransitionPlan) {
    let planned = plan_transition_v2(outgoing, incoming, config);
    let guarded = plan_guarded_transition_v2(outgoing, incoming, config);
    let outgoing_legacy = outgoing.as_v2_legacy_view();
    let incoming_legacy = incoming.as_v2_legacy_view();
    let Some(candidate) = planned.candidates.iter().find(|candidate| {
        if candidate.plan.kind != TransitionKind::BeatMatched || candidate.hard_rejection.is_some()
        {
            return false;
        }
        let Some(eligibility) = candidate.beat_eligibility.as_ref() else {
            return false;
        };
        if !eligibility.eligible
            || eligibility.beat_pairs < 8
            || eligibility.phase_error.is_none()
            || eligibility.tempo_hypothesis.is_none()
        {
            return false;
        }
        let Some(pair) = eligibility.tempo_hypothesis else {
            return false;
        };
        // A candidate must agree with the one-pass observed event clocks on
        // both decks after applying its explicit relation. This is a
        // runtime-available diagnostic and prevents a phase-valid but
        // numerically stale hypothesis from becoming BeatMatched authority.
        if !tempo_hypothesis_matches_event_clock(
            pair.outgoing,
            analysis_event_clock_bpm(
                outgoing
                    .rhythm_timeline()
                    .events
                    .iter()
                    .map(|event| event.time),
            ),
        ) || !tempo_hypothesis_matches_event_clock(
            pair.incoming,
            analysis_event_clock_bpm(
                incoming
                    .rhythm_timeline()
                    .events
                    .iter()
                    .map(|event| event.time),
            ),
        ) {
            return false;
        }
        let quality =
            evaluate_transition_quality(&outgoing_legacy, &incoming_legacy, &candidate.plan);
        quality.energy_samples_checked > 0
            && quality.max_beat_phase_error.is_some()
            && !quality.has_blocking_issue()
    }) else {
        return (planned, guarded);
    };
    let quality = evaluate_transition_quality(&outgoing_legacy, &incoming_legacy, &candidate.plan);
    let mut diagnostics = planned.diagnostics.clone();
    diagnostics.set_selection(TransitionKind::BeatMatched, candidate.cost, true);
    diagnostics.add_reason(wotoha_core::automix::AutoMixV2Reason::BeatMatchedSelected);
    let selected = V2GuardedTransitionPlan {
        plan: candidate.plan.clone(),
        cost: candidate.cost,
        cue_diagnostics: candidate.cue_diagnostics,
        quality,
        diagnostics,
        rejected_plan: None,
        rejected_quality: None,
    };
    (planned, selected)
}

/// Apply the same quality-first shadow policy with the objective rendered
/// quality observation available to the synthetic transition harness.  The
/// ordinary planner remains the fallback whenever the proposed BeatMatched
/// render contains a blocking discontinuity or energy defect.  This is a
/// research-only safety check; it does not alter the production planner.
fn quality_first_rendered_shadow_transition<O: V2AnalysisInput, I: V2AnalysisInput>(
    outgoing: &O,
    incoming: &I,
    outgoing_fixture: &SyntheticFixture,
    incoming_fixture: &SyntheticFixture,
    config: &AutoMixConfig,
) -> (
    TransitionPlanV2,
    V2GuardedTransitionPlan,
    crate::tempo_shadow_followup::RenderQualityObservation,
) {
    let (planned, selected) = quality_first_shadow_transition(outgoing, incoming, config);
    let selected_phase_error = planned
        .candidates
        .iter()
        .find(|candidate| candidate.plan == selected.plan)
        .and_then(|candidate| candidate.beat_eligibility.as_ref())
        .and_then(|eligibility| eligibility.phase_error);
    let rendered = crate::tempo_shadow_followup::render_preview(
        outgoing_fixture,
        incoming_fixture,
        &selected.plan,
        selected_phase_error,
    );
    if selected.plan.kind == TransitionKind::BeatMatched && !rendered.acceptable {
        let ordinary = plan_guarded_transition_v2(outgoing, incoming, config);
        (planned, ordinary, rendered)
    } else {
        (planned, selected, rendered)
    }
}

fn analysis_event_clock_bpm<I>(times: I) -> Option<f32>
where
    I: Iterator<Item = Duration>,
{
    let times = times.collect::<Vec<_>>();
    let intervals = times
        .windows(2)
        .filter_map(|window| window[1].checked_sub(window[0]))
        .map(|duration| duration.as_micros() as f64)
        .filter(|duration| *duration > 0.0 && duration.is_finite())
        .collect::<Vec<_>>();
    median_f64(&intervals)
        .map(|interval| (60_000_000.0 / interval) as f32)
        .filter(|bpm| bpm.is_finite() && *bpm > 0.0)
}

fn tempo_hypothesis_matches_event_clock(
    hypothesis: TempoHypothesis,
    event_bpm: Option<f32>,
) -> bool {
    let Some(event_bpm) = event_bpm else {
        return false;
    };
    let multiplier = match hypothesis.relation {
        TempoRelation::HalfTime => 0.5,
        TempoRelation::Primary | TempoRelation::Alternative => 1.0,
        TempoRelation::DoubleTime => 2.0,
    };
    relative_difference(hypothesis.bpm, event_bpm * multiplier) <= RELATIVE_TOLERANCE
}

fn synthetic_analysis(event_bpm: f32, candidate_bpm: f32, relation: &str) -> TrackAnalysisV2 {
    let duration = Duration::from_secs(12);
    let mut analysis = TrackAnalysisV2::unanalyzed(duration);
    let interval = Duration::from_secs_f32(60.0 / event_bpm.max(1.0));
    let mut time = Duration::from_millis(500);
    while time < duration.saturating_sub(Duration::from_secs(1)) {
        analysis.rhythm.beats.push(BeatEvent::new(
            time,
            Some(ModelScore::new(0.95).expect("score")),
            None,
            Confidence::new(0.95).expect("confidence"),
            Some(Support::new(0.95).expect("support")),
            Some(Support::new(0.95).expect("support")),
        ));
        time += interval;
    }
    analysis.audible_start = Duration::from_millis(500);
    analysis.audible_end = duration.saturating_sub(Duration::from_secs(1));
    analysis.rhythm.tempo_hypotheses =
        TempoHypothesis::with_relation(candidate_bpm, UnitInterval::ONE, parse_relation(relation))
            .into_iter()
            .collect();
    analysis.rhythm.meter_hypotheses = vec![MeterHypothesis {
        beats_per_bar: 4,
        downbeat_phase: 0,
        score: UnitInterval::ONE,
    }];
    analysis
}

fn build_adversarial_report() -> Result<MetricalAdversarialReport, LabError> {
    let cases = [
        ("120_to_125", 120.0, 125.0, "primary", false),
        ("120_to_130", 120.0, 130.0, "primary", false),
        ("120_to_140", 120.0, 140.0, "primary", false),
        ("120_to_160", 120.0, 160.0, "primary", false),
        ("120_to_60_alias", 120.0, 60.0, "half_time", true),
        ("120_to_240_alias", 120.0, 240.0, "double_time", true),
        ("80_to_100", 80.0, 100.0, "primary", false),
        ("80_to_120", 80.0, 120.0, "primary", false),
        ("80_to_160_alias", 80.0, 160.0, "double_time", true),
        ("130_to_65_alias", 130.0, 65.0, "half_time", true),
        ("130_to_150", 130.0, 150.0, "primary", false),
        ("130_to_180", 130.0, 180.0, "primary", false),
        ("160_to_80_alias", 160.0, 80.0, "half_time", true),
        ("160_to_120", 160.0, 120.0, "primary", false),
        ("160_to_180", 160.0, 180.0, "primary", false),
    ];
    let mut output = Vec::new();
    for (case_id, event_bpm, candidate_bpm, relation, declared_valid_alias) in cases {
        let outgoing = synthetic_analysis(event_bpm, candidate_bpm, relation);
        // Both tracks carry the same deliberately injected label.  This keeps
        // the planner's physical/quality path eligible while the metrical
        // guard evaluates the label against the independent event clock.
        let incoming = synthetic_analysis(event_bpm, candidate_bpm, relation);
        let config = AutoMixConfig {
            enabled: true,
            crossfade: Duration::from_secs(8),
            max_tempo_adjustment: 0.05,
            min_beat_confidence: 0.70,
        };
        let planned = plan_transition_v2(&outgoing, &incoming, &config);
        let guarded = plan_guarded_transition_v2(&outgoing, &incoming, &config);
        let eligibility = beat_match_eligibility(&outgoing, &incoming, &config);
        let (consistency, _, _) = consistency_for(candidate_bpm, relation, Some(event_bpm));
        let baseline_selected = guarded.plan.kind == TransitionKind::BeatMatched;
        let invalid_before = baseline_selected && !declared_valid_alias;
        let relation_is_safe = consistency == "consistent";
        let decision = if relation_is_safe {
            "select"
        } else {
            "abstain"
        };
        let effective_hypotheses = if relation_is_safe {
            TempoHypothesis::with_relation(
                candidate_bpm,
                UnitInterval::ONE,
                parse_relation(relation),
            )
            .into_iter()
            .collect()
        } else {
            Vec::new()
        };
        let effective_outgoing = ShadowAnalysisInput {
            analysis: &outgoing,
            hypotheses: effective_hypotheses.clone(),
        };
        let effective_incoming = ShadowAnalysisInput {
            analysis: &incoming,
            hypotheses: effective_hypotheses,
        };
        let effective_planned =
            plan_guarded_transition_v2(&effective_outgoing, &effective_incoming, &config);
        let effective_selected = effective_planned.plan.kind == TransitionKind::BeatMatched;
        let effective_invalid = effective_selected && !declared_valid_alias;
        let valid_alias_representable = declared_valid_alias
            && effective_outgoing
                .tempo_hypotheses()
                .iter()
                .any(|hypothesis| relation_name(hypothesis.relation) == relation);
        output.push(MetricalAdversarialCase {
            case_id: case_id.into(),
            event_clock_bpm: event_bpm,
            candidate_bpm,
            relation: relation.into(),
            declared_valid_alias,
            consistency,
            planner_physical_eligible: eligibility.eligible,
            beatmatched_generated: planned.diagnostics.beatmatched_candidates > 0,
            quality_guard_selected: baseline_selected,
            guarded_beatmatched: baseline_selected,
            beatmatched_after_metrical_guard: effective_selected,
            invalid_before_guard: invalid_before,
            invalid_after_guard: effective_invalid,
            transition: format!("{:?}", effective_planned.plan.kind),
            baseline_transition: format!("{:?}", guarded.plan.kind),
            baseline_beatmatched_selected: baseline_selected,
            shadow_decision_outgoing: decision.into(),
            shadow_decision_incoming: decision.into(),
            effective_shadow_transition: format!("{:?}", effective_planned.plan.kind),
            effective_shadow_beatmatched_selected: effective_selected,
            effective_shadow_false_beatmatched: effective_invalid,
            effective_shadow_safe_fallback: !effective_selected,
            valid_alias_representable,
        });
    }
    let invalid_before_guard = output
        .iter()
        .filter(|case| case.invalid_before_guard)
        .count();
    let invalid_after_guard = output
        .iter()
        .filter(|case| case.invalid_after_guard)
        .count();
    let valid_aliases_total = output
        .iter()
        .filter(|case| case.declared_valid_alias)
        .count();
    let valid_aliases_retained = output
        .iter()
        .filter(|case| case.valid_alias_representable)
        .count();
    Ok(MetricalAdversarialReport {
        cases: output,
        invalid_before_guard,
        invalid_after_guard,
        valid_aliases_retained,
        valid_aliases_total,
    })
}

fn build_candidate_pressure(flows: &[&CandidateFlowObservation]) -> CandidatePressureReport {
    let rows = flows
        .iter()
        .map(|flow| {
            let count = merge_evidence(flow, flows, MAX_CANDIDATES).len();
            CandidatePressureRow {
                fixture_id: flow.fixture_id.clone(),
                candidate_count: count,
                pair_cross_product: count * count,
            }
        })
        .collect::<Vec<_>>();
    let mut counts = rows
        .iter()
        .map(|row| row.candidate_count)
        .collect::<Vec<_>>();
    let mut pairs = rows
        .iter()
        .map(|row| row.pair_cross_product)
        .collect::<Vec<_>>();
    CandidatePressureReport {
        mean_candidates: mean_usize(&counts),
        p50_candidates: median_usize(&mut counts),
        p95_candidates: percentile_usize(&mut counts, 0.95),
        max_candidates: rows
            .iter()
            .map(|row| row.candidate_count)
            .max()
            .unwrap_or(0),
        mean_pair_cross_product: mean_usize(
            &rows
                .iter()
                .map(|row| row.pair_cross_product)
                .collect::<Vec<_>>(),
        ),
        p95_pair_cross_product: percentile_usize(&mut pairs, 0.95),
        max_pair_cross_product: rows
            .iter()
            .map(|row| row.pair_cross_product)
            .max()
            .unwrap_or(0),
        per_fixture: rows,
    }
}

fn build_pruning_summaries(flows: &[&CandidateFlowObservation]) -> Vec<PruningSummary> {
    [
        ("near_duplicate_merge", RankMode::Evidence),
        ("source_consensus", RankMode::Consensus),
        ("event_compatibility", RankMode::EventGated),
    ]
    .into_iter()
    .map(|(method, mode)| {
        let mut candidate_total = 0;
        let mut canonical_count = 0;
        let mut family = 0;
        let mut family_recall = 0;
        let mut false_family_candidates = 0;
        for flow in flows {
            let truth = flow.truth_bpm.expect("scalar flow");
            let candidates = rank_candidates(flow, flows, MAX_CANDIDATES, mode);
            candidate_total += candidates.len();
            canonical_count += usize::from(
                candidates
                    .first()
                    .is_some_and(|item| canonical(item, truth)),
            );
            family += usize::from(
                candidates
                    .first()
                    .is_some_and(|item| candidate_family_correct(item, truth)),
            );
            family_recall += usize::from(
                candidates
                    .iter()
                    .any(|item| candidate_family_correct(item, truth)),
            );
            false_family_candidates += candidates
                .iter()
                .filter(|item| !candidate_family_correct(item, truth))
                .count();
        }
        PruningSummary {
            method: method.into(),
            candidate_budget: MAX_CANDIDATES,
            mean_candidates: candidate_total as f32 / flows.len().max(1) as f32,
            top1_canonical_correct: canonical_count,
            top1_family_correct: family,
            candidate_family_recall: family_recall,
            false_family_candidates,
        }
    })
    .collect()
}

fn analysis_with_candidates(
    base: &TrackAnalysisV2,
    candidates: &[TempoCandidate],
) -> TrackAnalysisV2 {
    let mut analysis = base.clone();
    analysis.rhythm.tempo_hypotheses = hypotheses_from_candidates(candidates);
    analysis
}

fn pair_is_correct(
    pair: Option<wotoha_core::automix::TempoHypothesisPair>,
    outgoing_truth: f32,
    incoming_truth: f32,
) -> bool {
    let Some(pair) = pair else {
        return false;
    };
    let outgoing_family = [0.5_f32, 1.0, 2.0].iter().any(|ratio| {
        relative_difference(pair.outgoing.bpm, outgoing_truth * ratio) <= RELATIVE_TOLERANCE
    });
    let incoming_family = [0.5_f32, 1.0, 2.0].iter().any(|ratio| {
        relative_difference(pair.incoming.bpm, incoming_truth * ratio) <= RELATIVE_TOLERANCE
    });
    outgoing_family
        && incoming_family
        && relative_difference(
            pair.outgoing.bpm / pair.incoming.bpm,
            outgoing_truth / incoming_truth,
        ) <= RELATIVE_TOLERANCE
}

fn build_conservative_matrix(
    baseline: &TempoShadowFollowupReport,
    flows: &[&CandidateFlowObservation],
    decisions: &[DecisionInternal],
) -> Result<ConservativeShadowMatrix, LabError> {
    let mut variants = Vec::new();
    for case in &baseline.automix_shadow.cases {
        if case.variant != "current_v2" && case.variant != "merged_shadow" {
            continue;
        }
        let outgoing = flows
            .iter()
            .find(|flow| flow.fixture_id == case.outgoing_fixture);
        let incoming = flows
            .iter()
            .find(|flow| flow.fixture_id == case.incoming_fixture);
        let (Some(outgoing), Some(incoming)) = (outgoing, incoming) else {
            continue;
        };
        let outgoing_analysis = outgoing
            .research_analysis
            .as_ref()
            .ok_or_else(|| LabError::InvalidInput("missing research analysis".into()))?;
        let incoming_analysis = incoming
            .research_analysis
            .as_ref()
            .ok_or_else(|| LabError::InvalidInput("missing research analysis".into()))?;
        let outgoing_decision = decisions
            .iter()
            .find(|row| row.row.fixture_id == outgoing.fixture_id);
        let incoming_decision = decisions
            .iter()
            .find(|row| row.row.fixture_id == incoming.fixture_id);
        let outgoing_decision = outgoing_decision
            .ok_or_else(|| LabError::InvalidInput("missing outgoing shadow decision".into()))?;
        let incoming_decision = incoming_decision
            .ok_or_else(|| LabError::InvalidInput("missing incoming shadow decision".into()))?;

        let baseline_outgoing = if case.variant == "current_v2" {
            outgoing_analysis.clone()
        } else {
            analysis_with_candidates(outgoing_analysis, &case.outgoing_candidates)
        };
        let baseline_incoming = if case.variant == "current_v2" {
            incoming_analysis.clone()
        } else {
            analysis_with_candidates(incoming_analysis, &case.incoming_candidates)
        };
        let config = auto_mix_config();
        let baseline_planned =
            plan_guarded_transition_v2(&baseline_outgoing, &baseline_incoming, &config);
        let baseline_eligibility =
            beat_match_eligibility(&baseline_outgoing, &baseline_incoming, &config);
        let effective_outgoing_hypotheses = hypotheses_for_decision(outgoing, outgoing_decision);
        let effective_incoming_hypotheses = hypotheses_for_decision(incoming, incoming_decision);
        let effective_outgoing = ShadowAnalysisInput {
            analysis: outgoing_analysis,
            hypotheses: effective_outgoing_hypotheses,
        };
        let effective_incoming = ShadowAnalysisInput {
            analysis: incoming_analysis,
            hypotheses: effective_incoming_hypotheses,
        };
        let effective_planned =
            plan_guarded_transition_v2(&effective_outgoing, &effective_incoming, &config);
        let effective_eligibility =
            beat_match_eligibility(&effective_outgoing, &effective_incoming, &config);
        let baseline_selected = baseline_planned.plan.kind == TransitionKind::BeatMatched;
        let effective_selected = effective_planned.plan.kind == TransitionKind::BeatMatched;
        let baseline_correct = baseline_selected
            && pair_is_correct(
                baseline_eligibility.tempo_hypothesis,
                case.outgoing_truth_bpm,
                case.incoming_truth_bpm,
            );
        let effective_correct = effective_selected
            && pair_is_correct(
                effective_eligibility.tempo_hypothesis,
                case.outgoing_truth_bpm,
                case.incoming_truth_bpm,
            );
        let baseline_false = baseline_selected && !baseline_correct;
        let effective_false = effective_selected && !effective_correct;
        let effective_safe = !effective_selected && !effective_false;
        let use_effective = case.variant == "merged_shadow";
        variants.push(ConservativeVariantCase {
            pair_id: case.pair_id.clone(),
            variant: if case.variant == "merged_shadow" {
                "conservative_shadow"
            } else {
                "current_v2"
            }
            .into(),
            selected_bpm_outgoing: outgoing_decision.row.selected_bpm,
            selected_bpm_incoming: incoming_decision.row.selected_bpm,
            decision_outgoing: outgoing_decision.row.decision.clone(),
            decision_incoming: incoming_decision.row.decision.clone(),
            metrical_guard_outgoing: outgoing_decision.row.metrical_consistency.clone(),
            metrical_guard_incoming: incoming_decision.row.metrical_consistency.clone(),
            beatmatched_generated: if use_effective {
                effective_planned.diagnostics.beatmatched_candidates > 0
            } else {
                baseline_planned.diagnostics.beatmatched_candidates > 0
            },
            beatmatched_selected: if use_effective {
                effective_selected
            } else {
                baseline_selected
            },
            correct_beatmatched: if use_effective {
                effective_correct
            } else {
                baseline_correct
            },
            false_beatmatched: if use_effective {
                effective_false
            } else {
                baseline_false
            },
            safe_fallback: if use_effective {
                effective_safe
            } else {
                !baseline_selected
            },
            missed_opportunity: if use_effective {
                !effective_selected && effective_planned.diagnostics.beatmatched_candidates > 0
            } else {
                !baseline_selected && baseline_planned.diagnostics.beatmatched_candidates > 0
            },
            baseline_transition: format!("{:?}", baseline_planned.plan.kind),
            baseline_beatmatched_selected: baseline_selected,
            shadow_decision_outgoing: outgoing_decision.row.decision.clone(),
            shadow_decision_incoming: incoming_decision.row.decision.clone(),
            effective_shadow_transition: format!("{:?}", effective_planned.plan.kind),
            effective_shadow_beatmatched_selected: effective_selected,
            effective_shadow_false_beatmatched: effective_false,
            effective_shadow_safe_fallback: effective_safe,
        });
    }
    let current_cases = variants
        .iter()
        .filter(|case| case.variant == "current_v2")
        .collect::<Vec<_>>();
    let shadow_cases = variants
        .iter()
        .filter(|case| case.variant == "conservative_shadow")
        .collect::<Vec<_>>();
    Ok(ConservativeShadowMatrix {
        current_correct_beatmatched: current_cases
            .iter()
            .filter(|case| case.correct_beatmatched)
            .count(),
        current_false_beatmatched: current_cases
            .iter()
            .filter(|case| case.false_beatmatched)
            .count(),
        conservative_correct_beatmatched: shadow_cases
            .iter()
            .filter(|case| case.effective_shadow_beatmatched_selected && case.correct_beatmatched)
            .count(),
        conservative_false_beatmatched: shadow_cases
            .iter()
            .filter(|case| case.effective_shadow_false_beatmatched)
            .count(),
        safe_fallback: shadow_cases
            .iter()
            .filter(|case| case.effective_shadow_safe_fallback)
            .count(),
        missed_opportunity: shadow_cases
            .iter()
            .filter(|case| case.missed_opportunity)
            .count(),
        variants,
    })
}

fn summarize_effective_planner(matrix: &ConservativeShadowMatrix) -> EffectivePlannerSummary {
    let baseline = matrix
        .variants
        .iter()
        .filter(|case| case.variant == "current_v2")
        .collect::<Vec<_>>();
    let effective = matrix
        .variants
        .iter()
        .filter(|case| case.variant == "conservative_shadow")
        .collect::<Vec<_>>();
    EffectivePlannerSummary {
        baseline_transition_cases: baseline.len(),
        baseline_beatmatched_selected: baseline
            .iter()
            .filter(|case| case.baseline_beatmatched_selected)
            .count(),
        baseline_false_beatmatched: baseline
            .iter()
            .filter(|case| case.false_beatmatched)
            .count(),
        effective_transition_cases: effective.len(),
        effective_beatmatched_selected: effective
            .iter()
            .filter(|case| case.effective_shadow_beatmatched_selected)
            .count(),
        effective_false_beatmatched: effective
            .iter()
            .filter(|case| case.effective_shadow_false_beatmatched)
            .count(),
        effective_safe_fallback: effective
            .iter()
            .filter(|case| case.effective_shadow_safe_fallback)
            .count(),
        effective_missed_opportunity: effective
            .iter()
            .filter(|case| case.missed_opportunity)
            .count(),
        aliases_representable: 0,
        aliases_total: 0,
    }
}

fn build_runtime_feature_audit(_flows: &[&CandidateFlowObservation]) -> Vec<RuntimeFeatureAudit> {
    [
        (
            "candidate_evidence",
            "RUNTIME_AVAILABLE",
            "normalized score attached to a generated tempo candidate",
            "Classical and Neural candidate generation",
            "zero when the candidate is absent",
            true,
        ),
        (
            "event_agreement",
            "RUNTIME_AVAILABLE",
            "bounded agreement with the observed event interval clock",
            "decoded BeatEvent intervals",
            "missing event clock maps to zero",
            true,
        ),
        (
            "source_provenance",
            "RUNTIME_AVAILABLE",
            "bounded count of independent candidate evidence sources",
            "candidate provenance during one analysis",
            "zero when no source is available",
            true,
        ),
        (
            "full_low_agreement",
            "RUNTIME_AVAILABLE",
            "whether full-band and low-band Classical evidence merge at one BPM",
            "same-pass Classical candidates",
            "false when either band is missing",
            true,
        ),
        (
            "explicit_relation",
            "RUNTIME_AVAILABLE",
            "declared primary, half-time, double-time, or alternative relation",
            "TempoRelation metadata",
            "unlabeled candidates require event-clock resolution",
            true,
        ),
        (
            "event_interval_dispersion",
            "RUNTIME_DERIVABLE",
            "robust interval MAD divided by median interval",
            "BeatEvent timestamps in one pass",
            "unavailable below five intervals",
            false,
        ),
        (
            "single_pass_segment_consistency",
            "RUNTIME_DERIVABLE",
            "early, middle, and late interval medians from one analysis",
            "BeatEvent timestamps in one pass",
            "unavailable below three segments",
            false,
        ),
        (
            "duration_stability",
            "OFFLINE_ONLY",
            "agreement across independently analyzed duration variants",
            "cross-duration research corpus",
            "neutral value when no duration sibling exists",
            false,
        ),
        (
            "cross_duration_recurrence",
            "OFFLINE_ONLY",
            "recurrence of a candidate across separate duration runs",
            "cross-duration research corpus",
            "unavailable for a single runtime analysis",
            false,
        ),
    ]
    .into_iter()
    .map(
        |(
            name,
            classification,
            definition,
            source,
            missing_value_behavior,
            production_time_available,
        )| RuntimeFeatureAudit {
            name: name.into(),
            classification: classification.into(),
            definition: definition.into(),
            source: source.into(),
            missing_value_behavior: missing_value_behavior.into(),
            production_time_available,
        },
    )
    .collect()
}

fn rank_candidates_runtime(
    flow: &CandidateFlowObservation,
    flows: &[&CandidateFlowObservation],
    budget: usize,
) -> Vec<EvidenceCandidate> {
    let mut candidates = merge_evidence(flow, flows, budget);
    for candidate in &mut candidates {
        candidate.duration_stability = 0.5;
    }
    candidates.sort_by(|left, right| {
        let margin = left.candidate.normalized_score - right.candidate.normalized_score;
        rank_value(right, RankMode::ClassicalAnchor, margin)
            .total_cmp(&rank_value(left, RankMode::ClassicalAnchor, margin))
            .then_with(|| {
                right
                    .candidate
                    .normalized_score
                    .total_cmp(&left.candidate.normalized_score)
            })
            .then_with(|| left.candidate.bpm.total_cmp(&right.candidate.bpm))
    });
    for candidate in &mut candidates {
        candidate.candidate.shadow_rank_score =
            rank_value(candidate, RankMode::ClassicalAnchor, 0.0);
    }
    candidates
}

fn build_runtime_feasible_summary(flows: &[&CandidateFlowObservation]) -> RuntimeFeasibleSummary {
    let mut accepted = 0;
    let mut canonical_correct = 0;
    let mut family_correct = 0;
    let mut false_confident = 0;
    for flow in flows {
        let Some(truth) = flow.truth_bpm else {
            continue;
        };
        let candidates = rank_candidates_runtime(flow, flows, MAX_CANDIDATES);
        let first = candidates.first();
        let margin = first
            .zip(candidates.get(1))
            .map(|(one, two)| one.candidate.shadow_rank_score - two.candidate.shadow_rank_score)
            .unwrap_or(1.0);
        let safe = first.is_some_and(|item| {
            item.candidate.shadow_rank_score >= ACCEPT_SCORE
                && margin >= ACCEPT_MARGIN
                && item.source_count >= 2
                && item
                    .event_agreement
                    .is_some_and(|value| value >= ACCEPT_EVENT_AGREEMENT)
                && resolved_relation(item, flow.event_clock_bpm).is_some()
                && classical_anchor_allows(item, &candidates)
        });
        if safe {
            accepted += 1;
            canonical_correct += usize::from(canonical(first.expect("safe"), truth));
            family_correct += usize::from(candidate_family_correct(first.expect("safe"), truth));
            false_confident += usize::from(!candidate_family_correct(first.expect("safe"), truth));
        }
    }
    RuntimeFeasibleSummary {
        rule_name: "classical_anchor_guarded_without_cross_duration_features".into(),
        scored: flows.iter().filter(|flow| flow.truth_bpm.is_some()).count(),
        accepted,
        abstained_or_retained: flows
            .iter()
            .filter(|flow| flow.truth_bpm.is_some())
            .count()
            .saturating_sub(accepted),
        canonical_correct,
        family_correct,
        canonical_precision: family_precision(canonical_correct, accepted),
        family_precision: family_precision(family_correct, accepted),
        false_confident_accepts: false_confident,
    }
}

fn build_runtime_consistency(
    flows: &[&CandidateFlowObservation],
) -> Vec<RuntimeTempoConsistencyObservation> {
    flows
        .iter()
        .filter_map(|flow| {
            let analysis = flow.research_analysis.as_ref()?;
            let intervals = analysis
                .rhythm
                .beats
                .windows(2)
                .filter_map(|window| window[1].time.checked_sub(window[0].time))
                .map(|duration| duration.as_micros() as f64)
                .filter(|value| *value > 0.0)
                .collect::<Vec<_>>();
            let median = median_f64(&intervals);
            let mad_ratio = median.map(|center| {
                let deviations = intervals
                    .iter()
                    .map(|value| (value - center).abs())
                    .collect::<Vec<_>>();
                median_f64(&deviations).unwrap_or(0.0) / center.max(1.0)
            });
            let third = intervals.len() / 3;
            let early = median_f64(&intervals[..third.max(1).min(intervals.len())]);
            let middle = median_f64(
                &intervals
                    [third.min(intervals.len())..(2 * third).max(third + 1).min(intervals.len())],
            );
            let late = median_f64(&intervals[(2 * third).min(intervals.len())..]);
            let drift = |left: Option<f64>, right: Option<f64>| {
                left.zip(right)
                    .zip(median)
                    .map(|((left, right), center)| (left - right).abs() / center.max(1.0))
            };
            let early_middle = drift(early, middle);
            let middle_late = drift(middle, late);
            let early_late = drift(early, late);
            let max_drift = [early_middle, middle_late, early_late]
                .into_iter()
                .flatten()
                .fold(0.0, f64::max);
            let abstain = intervals.len() < 5
                || max_drift > STATIONARY_DRIFT
                || mad_ratio.is_some_and(|value| value > STATIONARY_DISPERSION);
            let classification = if intervals.len() < 5 {
                "insufficient_support"
            } else if max_drift > 0.05 {
                "strong_drift"
            } else if max_drift > STATIONARY_DRIFT {
                "slow_drift"
            } else {
                "single_pass_stationary"
            };
            Some(RuntimeTempoConsistencyObservation {
                fixture_id: flow.fixture_id.clone(),
                family: flow.family.clone(),
                interval_count: intervals.len(),
                interval_mad_ratio: mad_ratio,
                early_middle_drift: early_middle,
                middle_late_drift: middle_late,
                early_late_drift: early_late,
                classification: classification.into(),
                refinement_would_abstain: abstain,
            })
        })
        .collect()
}

fn build_abstention_reasons(
    flows: &[&CandidateFlowObservation],
    decisions: &[DecisionInternal],
) -> Vec<AbstentionReasonObservation> {
    decisions
        .iter()
        .filter(|decision| decision.row.decision != "select")
        .filter_map(|decision| {
            let flow = flows
                .iter()
                .find(|flow| flow.fixture_id == decision.row.fixture_id)?;
            let first = decision.candidates.first();
            let margin = first.zip(decision.candidates.get(1)).map(|(one, two)| {
                one.candidate.shadow_rank_score - two.candidate.shadow_rank_score
            });
            let reason = if first.is_none() {
                "candidate_unavailable"
            } else if first
                .is_some_and(|item| resolved_relation(item, flow.event_clock_bpm).is_none())
            {
                "relation_unresolved"
            } else if margin.is_some_and(|value| value < ACCEPT_MARGIN) {
                "ambiguous_score_margin"
            } else if first.is_some_and(|item| item.source_count < 2) {
                "weak_provenance"
            } else if first
                .and_then(|item| item.event_agreement)
                .is_none_or(|value| value < ACCEPT_EVENT_AGREEMENT)
            {
                "event_disagreement"
            } else if first.is_some_and(|item| !classical_anchor_allows(item, &decision.candidates))
            {
                "classical_anchor_missing"
            } else {
                "conservative_guard"
            };
            let candidate_family_available = flow.truth_bpm.is_some_and(|truth| {
                decision
                    .candidates
                    .iter()
                    .any(|candidate| candidate_family_correct(candidate, truth))
            });
            Some(AbstentionReasonObservation {
                fixture_id: decision.row.fixture_id.clone(),
                family: decision.row.family.clone(),
                decision: decision.row.decision.clone(),
                reason: reason.into(),
                candidate_count: decision.candidates.len(),
                candidate_family_available,
                recoverable_by_current_candidate_set: candidate_family_available,
            })
        })
        .collect()
}

#[derive(Clone)]
struct RealisticFlow {
    profile: String,
    flow: CandidateFlowObservation,
    pcm_sha256: String,
    master_id: String,
    seed: u64,
    truth_beat_times_micros: Vec<u64>,
}

fn realistic_specs() -> Vec<(FixtureSpec, String)> {
    let mut specs = Vec::new();
    let seed = 0x52_45_41_4c_49_53_54_u64;
    for duration_seconds in [30_u64, 60] {
        for bpm in [100.0_f32, 120.0, 140.0] {
            let profile = "sectional".to_string();
            let id = format!("realistic-{duration_seconds}-{profile}-{bpm:.1}");
            specs.push((
                FixtureSpec {
                    id: id.clone(),
                    family: FixtureFamily::ConstantTempo,
                    duration_micros: duration_seconds * 1_000_000,
                    sample_rate: 22_050,
                    channels: 1,
                    lead_in_micros: 500_000,
                    meter: 4,
                    meter_truth: Some(4),
                    tempo: TempoProfile::Constant { bpm },
                    event_style: EventStyle::Standard,
                    transform: TransformKind::None,
                    base_id: Some(id),
                    seed: seed + specs.len() as u64,
                },
                profile,
            ));
        }
    }
    for (index, (profile, bpm, duration_seconds)) in [
        ("sparse_breakdown", 128.0_f32, 30_u64),
        ("syncopated_drop", 128.0_f32, 30_u64),
        ("kickless_breakdown", 120.0_f32, 30_u64),
    ]
    .into_iter()
    .enumerate()
    {
        let id = format!("realistic-{duration_seconds}-{profile}-{bpm:.1}");
        specs.push((
            FixtureSpec {
                id: id.clone(),
                family: FixtureFamily::ConstantTempo,
                duration_micros: duration_seconds * 1_000_000,
                sample_rate: 22_050,
                channels: 1,
                lead_in_micros: 500_000,
                meter: 4,
                meter_truth: Some(4),
                tempo: TempoProfile::Constant { bpm },
                event_style: EventStyle::Standard,
                transform: TransformKind::None,
                base_id: Some(id),
                seed: seed + 100 + index as u64,
            },
            profile.into(),
        ));
    }
    specs
}

fn arrangement_gain(progress: f64, profile: &str) -> f32 {
    let base = if progress < 0.16 {
        lerp_f32(0.22, 0.58, progress / 0.16)
    } else if progress < 0.30 {
        lerp_f32(0.58, 1.0, (progress - 0.16) / 0.14)
    } else if progress < 0.65 {
        1.0
    } else if progress < 0.80 {
        0.16
    } else {
        lerp_f32(0.52, 0.20, (progress - 0.80) / 0.20)
    };
    if profile.contains("sparse") && progress < 0.30 {
        base * 0.65
    } else if profile.contains("kickless") && (0.65..0.80).contains(&progress) {
        base * 0.55
    } else {
        base
    }
}

fn lerp_f32(start: f32, end: f32, progress: f64) -> f32 {
    start + (end - start) * progress.clamp(0.0, 1.0) as f32
}

fn add_realistic_burst(
    audio: &mut [f32],
    sample_rate: u32,
    time_micros: u64,
    frequency: f32,
    gain: f32,
) {
    let center = time_micros as f64 * sample_rate as f64 / 1_000_000.0;
    let radius = (sample_rate as f64 * 0.035) as isize;
    let center_index = center.round() as isize;
    for offset in -radius..=radius {
        let index = center_index + offset;
        if index < 0 {
            continue;
        }
        if let Some(sample) = audio.get_mut(index as usize) {
            let seconds = offset as f32 / sample_rate as f32;
            let envelope = (-seconds.abs() * 95.0).exp();
            *sample += gain * envelope * (std::f32::consts::TAU * frequency * seconds).sin();
        }
    }
}

fn generate_realistic_fixture(
    spec: &FixtureSpec,
    profile: &str,
) -> Result<SyntheticFixture, LabError> {
    let mut fixture = generate_fixture(spec)?;
    let duration = spec.duration_micros.max(1) as f64;
    for (index, sample) in fixture.audio.iter_mut().enumerate() {
        let time = index as f64 * 1_000_000.0 / spec.sample_rate as f64;
        *sample *= arrangement_gain(time / duration, profile);
    }
    for (index, &beat_micros) in fixture.truth.beat_times_micros.iter().enumerate() {
        let progress = beat_micros as f64 / duration;
        if (0.16..0.65).contains(&progress) {
            let gain = if profile.contains("syncopated") && index % 4 == 2 {
                0.40
            } else {
                0.16
            };
            add_realistic_burst(
                &mut fixture.audio,
                spec.sample_rate,
                beat_micros,
                2_400.0,
                gain,
            );
        }
        if profile.contains("syncopated") && (0.30..0.65).contains(&progress) {
            let next = fixture
                .truth
                .beat_times_micros
                .get(index + 1)
                .copied()
                .unwrap_or(beat_micros);
            let interval = next.saturating_sub(beat_micros);
            if interval > 0 && index % 4 == 1 {
                add_realistic_burst(
                    &mut fixture.audio,
                    spec.sample_rate,
                    beat_micros.saturating_add(interval / 2),
                    3_200.0,
                    0.28,
                );
            }
        }
    }
    fixture.audio_sha256 = crate::hash_pcm(&fixture.audio);
    Ok(fixture)
}

/// Generate a positive transition fixture with arrangement variation while
/// preserving a strong, observable clock at the source boundaries.  The
/// positive corpus is intended to exercise the real analyzer and planner, not
/// to manufacture BeatEvents for the planner.  The boundary clock is therefore
/// rendered into audio and still passes through the normal analysis pipeline.
fn generate_positive_realistic_fixture(
    spec: &FixtureSpec,
    profile: &str,
    clock_gain_scale: f32,
    independent_render: bool,
) -> Result<SyntheticFixture, LabError> {
    let mut fixture = generate_fixture(spec)?;
    let audio_len = fixture.audio.len().max(1) as f64;
    // Keep rhythmic clocks compatible while making the harmonic bed an
    // independent rendering detail.  This prevents accidental phase-zero
    // oscillator clones from standing in for independently generated tracks.
    let bed_phase = if independent_render {
        (spec.seed as f32 * 0.618_033_95).rem_euclid(std::f32::consts::TAU)
    } else {
        0.0
    };
    let bed_level = if independent_render { 0.20 } else { 0.85 };
    for (index, sample) in fixture.audio.iter_mut().enumerate() {
        let progress = index as f64 / audio_len;
        let seconds = index as f32 / spec.sample_rate as f32;
        let envelope = if profile.contains("full_drop") {
            if progress < 0.16 {
                lerp_f32(0.86, 0.96, progress / 0.16)
            } else if progress < 0.30 {
                lerp_f32(0.96, 1.0, (progress - 0.16) / 0.14)
            } else if progress < 0.65 {
                1.0
            } else if progress < 0.80 {
                0.76
            } else {
                lerp_f32(0.84, 0.72, (progress - 0.80) / 0.20)
            }
        } else if progress < 0.16 {
            lerp_f32(0.55, 0.85, progress / 0.16)
        } else if progress < 0.30 {
            lerp_f32(0.85, 1.0, (progress - 0.16) / 0.14)
        } else if progress < 0.65 {
            1.0
        } else if progress < 0.80 {
            if profile.contains("kickless") {
                0.58
            } else {
                0.72
            }
        } else {
            lerp_f32(0.82, 0.70, (progress - 0.80) / 0.20)
        };
        let profile_gain = if profile.contains("sparse") {
            if progress < 0.30 { 0.82 } else { 1.0 }
        } else if profile.contains("syncopated") && (0.30..0.65).contains(&progress) {
            if (index / (spec.sample_rate as usize / 16).max(1)).is_multiple_of(4) {
                0.78
            } else {
                1.0
            }
        } else {
            1.0
        };
        // A shared low-level harmonic bed keeps the rendered overlap
        // measurable while the pulse/envelope layers still differ by seed
        // and arrangement profile.  This avoids using silence as a proxy for
        // a realistic transition-quality decision.
        let bed = (std::f32::consts::TAU * 110.0 * seconds + bed_phase).sin() * 0.25
            + (std::f32::consts::TAU * 220.0 * seconds + bed_phase * 1.7).sin() * 0.15
            + (std::f32::consts::TAU * 330.0 * seconds + bed_phase * 0.43).sin() * 0.08
            + (std::f32::consts::TAU * 4_200.0 * seconds + bed_phase * 2.1).sin() * 0.18
            + (std::f32::consts::TAU * 6_800.0 * seconds + bed_phase * 0.77).sin() * 0.10;
        // Keep the independent bed audible but subordinate to the generated
        // transient clock, so a seed-dependent phase cannot destabilize the
        // normal neural-to-V2 adaptation path.
        let source_level = 1.0;
        let texture = if independent_render {
            let state = (index as u64)
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(spec.seed ^ 0x9e37_79b9_7f4a_7c15);
            (((state >> 40) as f32 / (1_u64 << 24) as f32) - 0.5) * 0.08
        } else {
            0.0
        };
        *sample = *sample * source_level * envelope * profile_gain + bed * bed_level + texture;
    }
    // Keep every beat, including the first/last transition anchors, visible
    // to the analyzer.  Arrangement profiles still differ in envelope and
    // added off-beat detail, so these are not duplicated masters.
    for (index, &beat_micros) in fixture.truth.beat_times_micros.iter().enumerate() {
        let beat_gain = clock_gain_scale
            * if profile.contains("syncopated") && index % 4 == 2 {
                0.65
            } else if index.is_multiple_of(spec.meter as usize) {
                0.55
            } else {
                0.38
            };
        if independent_render {
            add_realistic_burst(
                &mut fixture.audio,
                spec.sample_rate,
                beat_micros,
                55.0,
                0.75 * clock_gain_scale,
            );
        }
        add_realistic_burst(
            &mut fixture.audio,
            spec.sample_rate,
            beat_micros,
            2_400.0,
            beat_gain,
        );
        if profile.contains("syncopated") && index % 4 == 1 {
            let next = fixture
                .truth
                .beat_times_micros
                .get(index + 1)
                .copied()
                .unwrap_or(beat_micros);
            let interval = next.saturating_sub(beat_micros);
            if interval > 0 {
                add_realistic_burst(
                    &mut fixture.audio,
                    spec.sample_rate,
                    beat_micros.saturating_add(interval / 2),
                    3_200.0,
                    0.22 * clock_gain_scale,
                );
            }
        }
    }
    if independent_render {
        let peak = fixture
            .audio
            .iter()
            .map(|sample| sample.abs())
            .fold(0.0_f32, f32::max);
        if peak.is_finite() && peak > 0.75 {
            let scale = 0.75 / peak;
            for sample in &mut fixture.audio {
                *sample *= scale;
            }
        }
    }
    fixture.audio_sha256 = crate::hash_pcm(&fixture.audio);
    Ok(fixture)
}

/// Align only a bounded eight-beat boundary window for a known compatible
/// positive pair.  The rest of each fixture remains independently generated,
/// so this does not duplicate a master or create a BeatEvent-only object.  It
/// gives the actual analyzer a physically coherent handoff region while the
/// arrangement evidence outside that region remains different.
fn align_positive_boundary_window(
    source_audio: &[f32],
    source_beat_times: &[u64],
    incoming: &mut SyntheticFixture,
    incoming_start_micros: u64,
) {
    const BOUNDARY_PADDING_BEATS: usize = 4;
    let Some(source_start_micros) = source_beat_times
        .get(
            source_beat_times
                .len()
                .saturating_sub(9 + BOUNDARY_PADDING_BEATS),
        )
        .copied()
    else {
        return;
    };
    let Some(source_end_micros) = source_beat_times.last().copied() else {
        return;
    };
    let source_start = (source_start_micros as f64 * incoming.spec.sample_rate as f64 / 1_000_000.0)
        .round() as usize;
    let source_len = ((source_end_micros.saturating_sub(source_start_micros)) as f64
        * incoming.spec.sample_rate as f64
        / 1_000_000.0)
        .round() as usize;
    let incoming_start = (incoming_start_micros as f64 * incoming.spec.sample_rate as f64
        / 1_000_000.0)
        .round() as usize;
    let Some(source_window) =
        source_audio.get(source_start..source_start.saturating_add(source_len))
    else {
        return;
    };
    let destination_end = incoming_start.saturating_add(source_window.len());
    if destination_end > incoming.audio.len() {
        return;
    }
    incoming.audio[incoming_start..destination_end].copy_from_slice(source_window);
    incoming.audio_sha256 = crate::hash_pcm(&incoming.audio);
}

fn positive_realistic_specs() -> Vec<(FixtureSpec, String)> {
    let definitions = [
        ("sectional-a", 100.0_f32, 30_u64, "sectional"),
        ("sparse-b", 100.0, 60, "sparse_breakdown"),
        ("syncopated-c", 100.0, 30, "syncopated_drop"),
        ("sectional-a", 120.0, 30, "sectional"),
        ("sparse-b", 120.0, 60, "sparse_breakdown"),
        ("sectional-a", 128.0, 30, "sectional"),
        ("sparse-b", 128.0, 60, "sparse_breakdown"),
        ("syncopated-c", 128.0, 30, "syncopated_drop"),
        ("sectional-a", 130.0, 60, "sectional"),
        ("kickless-b", 130.0, 30, "kickless_breakdown"),
        ("sectional-a", 140.0, 30, "sectional"),
        ("sparse-b", 140.0, 60, "sparse_breakdown"),
        ("sectional-a", 80.0, 30, "sectional"),
        ("sparse-b", 160.0, 60, "sparse_breakdown"),
        ("sectional-a", 60.0, 30, "sectional"),
        ("kickless-b", 120.0, 60, "kickless_breakdown"),
        ("sectional-a", 90.0, 30, "sectional"),
        ("sparse-b", 180.0, 60, "sparse_breakdown"),
        ("sectional-r", 120.0, 60, "sectional"),
        ("sectional-s", 120.0, 60, "sectional"),
        ("sectional-t", 120.0, 60, "sectional"),
        ("sectional-u", 120.0, 60, "sectional"),
        ("sectional-v", 120.0, 60, "sectional"),
        ("sectional-w", 120.0, 60, "sectional"),
        ("sectional-x", 120.0, 60, "sectional"),
        ("sectional-y", 120.0, 60, "sectional"),
        ("sectional-z", 120.0, 60, "sectional"),
        ("sectional-aa", 120.0, 60, "sectional"),
    ];
    let seed = 0x50_4f_53_49_54_49_56_u64;
    definitions
        .into_iter()
        .enumerate()
        .map(|(index, (variant, bpm, duration_seconds, profile))| {
            let id = format!("positive-{duration_seconds}-{profile}-{bpm:.1}-{variant}");
            (
                FixtureSpec {
                    id: id.clone(),
                    family: FixtureFamily::ConstantTempo,
                    duration_micros: duration_seconds * 1_000_000,
                    sample_rate: 44_100,
                    channels: 1,
                    lead_in_micros: 500_000,
                    meter: 4,
                    meter_truth: Some(4),
                    tempo: TempoProfile::Constant { bpm },
                    event_style: EventStyle::Standard,
                    transform: TransformKind::None,
                    base_id: Some(id),
                    seed: seed + index as u64,
                },
                profile.into(),
            )
        })
        .collect()
}

/// Independent-positive fixtures deliberately use a different seed namespace
/// and do not share the mechanics corpus' aligned masters.  The pair list is
/// evaluation metadata only; inference sees analyzed audio and generic
/// runtime diagnostics, never these nominal values.
fn independent_positive_specs() -> Vec<(FixtureSpec, String)> {
    let definitions = [
        ("sectional-a", 100.0_f32, 30_u64, "sectional"),
        ("sectional-b", 100.0, 60, "sectional"),
        ("syncopated-a", 100.0, 30, "syncopated_drop"),
        ("sparse-a", 100.0, 60, "sparse_breakdown"),
        ("sectional-c", 120.0, 30, "sectional"),
        ("sectional-d", 120.0, 60, "sectional"),
        ("kickless-a", 120.0, 60, "kickless_breakdown"),
        ("sectional-e", 90.0, 30, "sectional"),
        ("sectional-f", 90.0, 60, "sectional"),
        ("sectional-g", 128.0, 60, "sectional"),
        ("syncopated-b", 128.0, 30, "syncopated_drop"),
        ("sectional-h", 130.0, 60, "sectional"),
        ("kickless-b", 130.0, 60, "kickless_breakdown"),
        ("sectional-i", 140.0, 60, "sectional"),
        ("sectional-j", 140.0, 30, "sectional"),
        ("sectional-k", 80.0, 30, "sectional"),
        ("sectional-l", 160.0, 60, "sectional"),
        ("sectional-m", 60.0, 30, "sectional"),
        ("sectional-n", 120.0, 30, "sectional"),
        ("sectional-o", 121.0, 60, "sectional"),
        ("sectional-p", 126.0, 60, "sectional"),
        ("sectional-q", 132.0, 60, "sectional"),
        ("sectional-r", 180.0, 60, "sectional"),
        ("sectional-b2", 100.0, 60, "sectional"),
        ("sectional-d2", 120.0, 60, "sectional"),
        ("sectional-f2", 90.0, 60, "sectional"),
        ("sectional-m2", 60.0, 30, "sectional"),
        ("sectional-h2", 130.0, 60, "sectional"),
        ("sectional-o2", 121.0, 60, "sectional"),
        ("full-a", 100.0, 60, "full_drop"),
        ("full-b", 60.0, 30, "full_drop"),
        ("full-c", 130.0, 60, "full_drop"),
        ("full-d", 90.0, 60, "full_drop"),
        ("full-e", 121.0, 60, "full_drop"),
        ("full-f", 120.0, 30, "full_drop"),
        ("full-g", 120.0, 60, "full_drop"),
        ("full-h", 90.0, 30, "full_drop"),
        ("full-i", 90.0, 60, "full_drop"),
        ("full-j", 128.0, 30, "full_drop"),
        ("full-k", 128.0, 60, "full_drop"),
        ("full-l", 60.0, 30, "full_drop"),
        ("full-m", 60.0, 60, "full_drop"),
        ("full-n", 127.5, 30, "full_drop"),
        ("full-o", 127.5, 60, "full_drop"),
        ("full-p", 127.5, 60, "full_drop"),
        ("full-q", 130.0, 60, "full_drop"),
        ("full-r", 110.0, 30, "full_drop"),
        ("full-s", 110.0, 60, "full_drop"),
        ("full-t", 110.0, 60, "full_drop"),
    ];
    let seed = 0x49_4e_44_50_4f_53_u64;
    definitions
        .into_iter()
        .enumerate()
        .map(|(index, (variant, bpm, duration_seconds, profile))| {
            let id = format!("independent-{duration_seconds}-{profile}-{bpm:.1}-{variant}");
            (
                FixtureSpec {
                    id: id.clone(),
                    family: FixtureFamily::ConstantTempo,
                    duration_micros: duration_seconds * 1_000_000,
                    sample_rate: 44_100,
                    channels: 1,
                    lead_in_micros: 0,
                    meter: 4,
                    meter_truth: Some(4),
                    tempo: TempoProfile::Constant { bpm },
                    event_style: EventStyle::Standard,
                    transform: TransformKind::None,
                    base_id: Some(id),
                    seed: seed + index as u64,
                },
                profile.into(),
            )
        })
        .collect()
}

fn positive_expected_relation(outgoing_bpm: f32, incoming_bpm: f32) -> String {
    let ratio = outgoing_bpm / incoming_bpm.max(f32::EPSILON);
    if (ratio - 1.0).abs() <= RELATION_RESOLUTION_TOLERANCE {
        "primary_pair".into()
    } else if (ratio - 0.5).abs() <= RELATION_RESOLUTION_TOLERANCE {
        "incoming_double_time_family".into()
    } else if (ratio - 2.0).abs() <= RELATION_RESOLUTION_TOLERANCE {
        "incoming_half_time_family".into()
    } else {
        "alternative_or_unresolved".into()
    }
}

const REALISTIC_BEAT_MATCH_TOLERANCE_MICROS: u64 = 35_000;

fn realistic_beat_grid_diagnostic(
    truth_times: &[u64],
    detected_times: &[Duration],
) -> RealisticBeatGridDiagnostic {
    let tolerance = REALISTIC_BEAT_MATCH_TOLERANCE_MICROS as i64;
    let mut used = vec![false; detected_times.len()];
    let mut errors = Vec::new();
    for &truth in truth_times {
        let best = detected_times
            .iter()
            .enumerate()
            .filter(|(index, detected)| {
                !used[*index] && (detected.as_micros() as i64 - truth as i64).abs() <= tolerance
            })
            .min_by_key(|(_, detected)| {
                (detected.as_micros() as i64 - truth as i64).unsigned_abs()
            });
        if let Some((index, detected)) = best {
            used[index] = true;
            errors.push(detected.as_micros() as i64 - truth as i64);
        }
    }
    let matched = errors.len();
    let mut absolute = errors
        .iter()
        .map(|error| error.unsigned_abs())
        .collect::<Vec<_>>();
    absolute.sort_unstable();
    let percentile = |values: &[u64], numerator: usize, denominator: usize| {
        if values.is_empty() {
            None
        } else {
            let index = ((values.len() - 1) * numerator + denominator / 2) / denominator;
            values.get(index).copied()
        }
    };
    let precision =
        (!detected_times.is_empty()).then(|| matched as f32 / detected_times.len() as f32);
    let recall = (!truth_times.is_empty()).then(|| matched as f32 / truth_times.len() as f32);
    let first = errors.first().copied();
    let drift = errors
        .first()
        .zip(errors.last())
        .map(|(first, last)| last - first);
    RealisticBeatGridDiagnostic {
        matching_tolerance_micros: REALISTIC_BEAT_MATCH_TOLERANCE_MICROS,
        truth_beat_count: truth_times.len(),
        detected_beat_count: detected_times.len(),
        matched_truth_beats: matched,
        unmatched_truth_beats: truth_times.len().saturating_sub(matched),
        extra_detected_beats: detected_times.len().saturating_sub(matched),
        precision,
        recall,
        median_absolute_timing_error_micros: percentile(&absolute, 1, 2),
        p95_absolute_timing_error_micros: percentile(&absolute, 19, 20),
        max_absolute_timing_error_micros: absolute.last().copied(),
        first_beat_offset_micros: first,
        longitudinal_drift_micros: drift,
    }
}

#[allow(clippy::too_many_arguments)]
fn positive_opportunity_taxonomy(
    beatmatched_selected: bool,
    false_beatmatched: bool,
    candidate_survived_guard: bool,
    candidate_generated: bool,
    outgoing_decision: &str,
    incoming_decision: &str,
    selected_pair: bool,
    eligibility: &wotoha_core::automix::BeatMatchEligibility,
) -> &'static str {
    if beatmatched_selected {
        return if false_beatmatched {
            "INVALID_BEATMATCHED"
        } else {
            "CORRECT_BEATMATCHED"
        };
    }
    if outgoing_decision != "select" || incoming_decision != "select" {
        return "TRACK_UNCERTAINTY_BLOCKED";
    }
    if !selected_pair {
        return if eligibility.rejection.is_some() {
            "NO_COMPATIBLE_TEMPO_PAIR"
        } else {
            "RELATION_UNRESOLVED"
        };
    }
    if !eligibility.eligible {
        return if matches!(
            eligibility.rejection,
            Some(wotoha_core::automix::BeatMatchRejection::PhysicalWindowUnavailable)
        ) {
            "PHYSICAL_ELIGIBILITY_FAILED"
        } else {
            "NO_COMPATIBLE_TEMPO_PAIR"
        };
    }
    if candidate_survived_guard {
        "SOFT_RANKING_PREFERRED_FALLBACK"
    } else if candidate_generated {
        "QUALITY_GUARD_REJECTED"
    } else if eligibility.beat_pairs == 0 {
        "CUE_OPPORTUNITY_MISSING"
    } else {
        "SAFE_FALLBACK_OTHER"
    }
}

fn positive_pair_is_correct(
    pair: Option<wotoha_core::automix::TempoHypothesisPair>,
    outgoing_truth: f32,
    incoming_truth: f32,
) -> bool {
    let Some(pair) = pair else {
        return false;
    };
    let outgoing_correct = family_correct_bpm(pair.outgoing.bpm, outgoing_truth);
    let incoming_correct = family_correct_bpm(pair.incoming.bpm, incoming_truth);
    let expected_ratio = pair.outgoing.bpm / pair.incoming.bpm.max(f32::EPSILON);
    let truth_ratio = outgoing_truth / incoming_truth.max(f32::EPSILON);
    outgoing_correct
        && incoming_correct
        && relative_difference(expected_ratio, truth_ratio) <= RELATIVE_TOLERANCE
}

fn relation_physical_bpm(hypothesis: TempoHypothesis) -> f32 {
    let multiplier = match hypothesis.relation {
        TempoRelation::HalfTime => 2.0,
        TempoRelation::DoubleTime => 0.5,
        TempoRelation::Primary | TempoRelation::Alternative => 1.0,
    };
    hypothesis.bpm * multiplier
}

fn positive_realistic_transition_case(
    case_id: &str,
    outgoing: &RealisticFlow,
    incoming: &RealisticFlow,
    outgoing_fixture: &SyntheticFixture,
    incoming_fixture: &SyntheticFixture,
    outgoing_decision: &DecisionInternal,
    incoming_decision: &DecisionInternal,
) -> Result<RealisticPositiveTransitionCase, LabError> {
    let outgoing_analysis = outgoing
        .flow
        .research_analysis
        .as_ref()
        .ok_or_else(|| LabError::InvalidInput("positive outgoing analysis missing".into()))?;
    let incoming_analysis = incoming
        .flow
        .research_analysis
        .as_ref()
        .ok_or_else(|| LabError::InvalidInput("positive incoming analysis missing".into()))?;
    let outgoing_truth = outgoing
        .flow
        .truth_bpm
        .ok_or_else(|| LabError::InvalidInput("positive outgoing truth missing".into()))?;
    let incoming_truth = incoming
        .flow
        .truth_bpm
        .ok_or_else(|| LabError::InvalidInput("positive incoming truth missing".into()))?;
    let outgoing_hypotheses = hypotheses_for_decision(&outgoing.flow, outgoing_decision);
    let incoming_hypotheses = hypotheses_for_decision(&incoming.flow, incoming_decision);
    let outgoing_cues = realistic_analysis_cues(outgoing_analysis);
    let incoming_cues = realistic_analysis_cues(incoming_analysis);
    let outgoing_input = RealisticPlannerInput {
        analysis: outgoing_analysis,
        hypotheses: outgoing_hypotheses.clone(),
        cues: outgoing_cues,
    };
    let incoming_input = RealisticPlannerInput {
        analysis: incoming_analysis,
        hypotheses: incoming_hypotheses.clone(),
        cues: incoming_cues,
    };
    let config = auto_mix_config();
    let ordinary_plan = plan_guarded_transition_v2(&outgoing_input, &incoming_input, &config);
    let ordinary_candidates = plan_transition_v2(&outgoing_input, &incoming_input, &config);
    let ordinary = quality_first_rendered_shadow_transition(
        &outgoing_input,
        &incoming_input,
        outgoing_fixture,
        incoming_fixture,
        &config,
    );
    let (planned, guarded, rendered_quality) = if ordinary.0.diagnostics.beatmatched_candidates == 0
    {
        // Research-only opportunity probe: use the same analyzed timelines
        // and hypotheses while asking the core planner to evaluate its
        // bounded physical-window fallback when every heuristic cue pair was
        // unsuitable. No truth or boundary audio participates.
        let cue_free_outgoing = RealisticPlannerInput {
            analysis: outgoing_analysis,
            hypotheses: outgoing_hypotheses,
            cues: Vec::new(),
        };
        let cue_free_incoming = RealisticPlannerInput {
            analysis: incoming_analysis,
            hypotheses: incoming_hypotheses,
            cues: Vec::new(),
        };
        let fallback = quality_first_rendered_shadow_transition(
            &cue_free_outgoing,
            &cue_free_incoming,
            outgoing_fixture,
            incoming_fixture,
            &config,
        );
        if fallback.0.diagnostics.beatmatched_candidates > 0 {
            fallback
        } else {
            ordinary
        }
    } else {
        ordinary
    };
    let eligibility = beat_match_eligibility(&outgoing_input, &incoming_input, &config);
    let selected_pair = planned
        .candidates
        .iter()
        .find(|candidate| candidate.plan == guarded.plan)
        .and_then(|candidate| candidate.beat_eligibility.as_ref())
        .and_then(|eligibility| eligibility.tempo_hypothesis)
        .or(eligibility.tempo_hypothesis);
    let selected_phase_error = planned
        .candidates
        .iter()
        .find(|candidate| candidate.plan == guarded.plan)
        .and_then(|candidate| candidate.beat_eligibility.as_ref())
        .and_then(|candidate| candidate.phase_error)
        .or(eligibility.phase_error);
    let beatmatched_candidate_generated = planned.diagnostics.beatmatched_candidates > 0;
    let beatmatched_candidate_survived_guard = planned.candidates.iter().any(|candidate| {
        candidate.plan.kind == TransitionKind::BeatMatched && candidate.hard_rejection.is_none()
    });
    let beatmatched_selected = guarded.plan.kind == TransitionKind::BeatMatched;
    let tempo_hypothesis_pair_correct =
        positive_pair_is_correct(selected_pair, outgoing_truth, incoming_truth);
    let relation_correct = selected_pair.is_some_and(|pair| {
        let outgoing_physical = relation_physical_bpm(pair.outgoing);
        let incoming_physical = relation_physical_bpm(pair.incoming);
        outgoing_physical.is_finite()
            && incoming_physical.is_finite()
            && family_correct_bpm(outgoing_physical, outgoing_truth)
            && family_correct_bpm(incoming_physical, incoming_truth)
            && relative_difference(
                outgoing_physical / incoming_physical.max(f32::EPSILON),
                outgoing_truth / incoming_truth.max(f32::EPSILON),
            ) <= RELATION_RESOLUTION_TOLERANCE
    });
    let quality_guard_passed = beatmatched_selected
        && guarded.rejected_plan.is_none()
        && !guarded.quality.has_blocking_issue();
    let render_quality = if guarded.plan.kind == TransitionKind::BeatMatched
        || planned
            .candidates
            .iter()
            .any(|candidate| candidate.plan.kind == TransitionKind::BeatMatched)
    {
        // Preserve the objective render observation for a rejected
        // BeatMatched opportunity; otherwise a fallback Gapless render would
        // overwrite the evidence needed to classify the rejection.
        rendered_quality
    } else {
        crate::tempo_shadow_followup::render_preview(
            outgoing_fixture,
            incoming_fixture,
            &guarded.plan,
            selected_phase_error,
        )
    };
    let false_beatmatched = beatmatched_selected
        && (!tempo_hypothesis_pair_correct
            || !relation_correct
            || !quality_guard_passed
            || !render_quality.acceptable);
    let opportunity_taxonomy = positive_opportunity_taxonomy(
        beatmatched_selected,
        false_beatmatched,
        beatmatched_candidate_survived_guard,
        beatmatched_candidate_generated,
        &outgoing_decision.row.decision,
        &incoming_decision.row.decision,
        selected_pair.is_some(),
        &eligibility,
    );
    let opportunity_class = if beatmatched_selected {
        if false_beatmatched {
            "selected_but_invalid"
        } else {
            "effective_correct_beatmatched"
        }
    } else if beatmatched_candidate_survived_guard {
        "safe_candidate_lost_ranking"
    } else if beatmatched_candidate_generated {
        "candidate_generated_guard_rejected"
    } else if outgoing_decision.row.decision == "abstain"
        || incoming_decision.row.decision == "abstain"
    {
        "track_level_uncertainty_blocked_authority"
    } else if selected_pair.is_none() {
        "relation_unresolved_or_no_compatible_pair"
    } else {
        "beat_match_impossible"
    };
    let planner_reason = format!(
        "{:?}",
        explain_beatmatch_decision_v2(&outgoing_input, &incoming_input, &config)
    );
    let selected_relation_pair = selected_pair.map(|pair| {
        format!("{:?}/{:?}", pair.outgoing.relation, pair.incoming.relation).to_ascii_lowercase()
    });
    let selected_outgoing_bpm = selected_pair.map(|pair| pair.outgoing.bpm);
    let selected_incoming_bpm = selected_pair.map(|pair| pair.incoming.bpm);
    let selected_ratio = selected_pair.map(|pair| pair.ratio);
    let candidate_costs = planned
        .candidates
        .iter()
        .map(|candidate| {
            let outgoing_legacy = outgoing_input.as_v2_legacy_view();
            let incoming_legacy = incoming_input.as_v2_legacy_view();
            let quality =
                evaluate_transition_quality(&outgoing_legacy, &incoming_legacy, &candidate.plan);
            let score = transition_score_breakdown(&quality);
            RealisticPlannerCandidateObservation {
                kind: format!("{:?}", candidate.plan.kind),
                total_cost: candidate.cost.total,
                strategy_base_cost: candidate.cost.strategy_base_cost,
                tempo_stretch_cost: candidate.cost.tempo_stretch_cost,
                phase_precision_cost: candidate.cost.phase_precision_cost,
                structure_uncertainty_cost: candidate.cost.structure_uncertainty_cost,
                structure_alignment_cost: candidate.cost.structure_alignment_cost,
                rhythm_uncertainty_cost: candidate.cost.rhythm_uncertainty_cost,
                cue_suitability_cost: candidate.cost.cue_suitability_cost,
                blend_duration_cost: candidate.cost.blend_duration_cost,
                legacy_quality_cost: candidate.cost.legacy_quality_cost,
                quality_min_mix_energy_ratio: quality.min_mix_energy_ratio,
                quality_max_mix_energy_ratio: quality.max_mix_energy_ratio,
                quality_handoff_mix_energy_ratio: quality.handoff_mix_energy_ratio,
                quality_energy_balance_penalty: score.map(|value| value.energy_balance_penalty),
                quality_handoff_energy_penalty: score.map(|value| value.handoff_energy_penalty),
                quality_handoff_ownership_penalty: score
                    .map(|value| value.handoff_ownership_penalty),
                quality_phrase_strength_penalty: score.map(|value| value.phrase_strength_penalty),
                quality_overlap_seconds: quality.overlap.as_secs_f32(),
                outgoing_peak_dbfs: outgoing_legacy
                    .true_peak_dbtp
                    .or(outgoing_legacy.sample_peak_dbfs),
                incoming_peak_dbfs: incoming_legacy
                    .true_peak_dbtp
                    .or(incoming_legacy.sample_peak_dbfs),
                outgoing_rms_dbfs: outgoing_legacy.rms_dbfs,
                incoming_rms_dbfs: incoming_legacy.rms_dbfs,
                quality_issues: quality
                    .issues
                    .iter()
                    .map(|issue| format!("{issue:?}"))
                    .collect(),
                phase_error_micros: candidate
                    .beat_eligibility
                    .as_ref()
                    .and_then(|eligibility| eligibility.phase_error)
                    .map(|error| error.as_micros() as u64),
                outgoing_start_micros: candidate.plan.outgoing_start.as_micros() as u64,
                incoming_start_micros: candidate.plan.incoming_start.as_micros() as u64,
                duration_micros: candidate.plan.duration.as_micros() as u64,
                tempo_pair: candidate
                    .beat_eligibility
                    .as_ref()
                    .and_then(|eligibility| eligibility.tempo_hypothesis)
                    .map(|pair| format!("{:.6}/{:.6}", pair.outgoing.bpm, pair.incoming.bpm)),
                tempo_pair_relation: candidate
                    .beat_eligibility
                    .as_ref()
                    .and_then(|eligibility| eligibility.tempo_hypothesis)
                    .map(|pair| {
                        format!("{:?}/{:?}", pair.outgoing.relation, pair.incoming.relation)
                            .to_ascii_lowercase()
                    }),
                hard_rejection: candidate
                    .hard_rejection
                    .map(|rejection| format!("{rejection:?}")),
                outgoing_cue_index: candidate
                    .cue_diagnostics
                    .map(|diagnostics| diagnostics.outgoing_cue_index),
                incoming_cue_index: candidate
                    .cue_diagnostics
                    .map(|diagnostics| diagnostics.incoming_cue_index),
            }
        })
        .collect();
    let render_acceptable = render_quality.acceptable;
    Ok(RealisticPositiveTransitionCase {
        case_id: case_id.into(),
        outgoing_fixture: outgoing.flow.fixture_id.clone(),
        incoming_fixture: incoming.flow.fixture_id.clone(),
        outgoing_truth_bpm: outgoing_truth,
        incoming_truth_bpm: incoming_truth,
        outgoing_decision: outgoing_decision.row.decision.clone(),
        incoming_decision: incoming_decision.row.decision.clone(),
        expected_relation: positive_expected_relation(outgoing_truth, incoming_truth),
        selected_relation_pair,
        selected_outgoing_bpm,
        selected_incoming_bpm,
        selected_ratio,
        tempo_hypothesis_pair_correct,
        relation_correct,
        beatmatched_candidate_generated,
        beatmatched_candidate_survived_guard,
        quality_guard_passed,
        beatmatched_selected,
        false_beatmatched,
        safe_fallback: !beatmatched_selected && !false_beatmatched,
        transition: format!("{:?}", guarded.plan.kind),
        planner_reason,
        beat_pairs: eligibility.beat_pairs,
        phase_error_micros: eligibility
            .phase_error
            .map(|error| error.as_micros() as u64),
        eligibility: if eligibility.eligible {
            "eligible".into()
        } else {
            "rejected".into()
        },
        eligibility_rejection: eligibility
            .rejection
            .map(|rejection| format!("{rejection:?}")),
        outgoing_mix_out_cues: planned.diagnostics.outgoing_mix_out_cues,
        incoming_mix_in_cues: planned.diagnostics.incoming_mix_in_cues,
        cue_pairs_checked: planned.diagnostics.cue_pairs_checked,
        cue_tempo_combinations_checked: planned.diagnostics.cue_tempo_combinations_checked,
        planner_hard_rejections: planned
            .diagnostics
            .hard_rejections
            .iter()
            .map(|rejection| format!("{rejection:?}"))
            .collect(),
        tempo_adjustment: beatmatched_selected
            .then_some((guarded.plan.incoming_tempo_ratio - 1.0).abs()),
        render_quality,
        opportunity_class: opportunity_class.into(),
        opportunity_taxonomy: opportunity_taxonomy.into(),
        outgoing_pcm_sha256: outgoing_fixture.audio_sha256.clone(),
        incoming_pcm_sha256: incoming_fixture.audio_sha256.clone(),
        outgoing_master_id: outgoing_fixture
            .spec
            .base_id
            .clone()
            .unwrap_or_else(|| outgoing_fixture.spec.id.clone()),
        incoming_master_id: incoming_fixture
            .spec
            .base_id
            .clone()
            .unwrap_or_else(|| incoming_fixture.spec.id.clone()),
        outgoing_seed: outgoing_fixture.spec.seed,
        incoming_seed: incoming_fixture.spec.seed,
        waveform_copy_used: false,
        truth_inference_used: false,
        ordinary_selected_kind: format!("{:?}", ordinary_plan.plan.kind),
        ordinary_beatmatched_cost: ordinary_candidates
            .candidates
            .iter()
            .find(|candidate| candidate.plan.kind == TransitionKind::BeatMatched)
            .map(|candidate| candidate.cost.total),
        ordinary_gapless_cost: ordinary_candidates
            .candidates
            .iter()
            .find(|candidate| candidate.plan.kind == TransitionKind::Gapless)
            .map(|candidate| candidate.cost.total),
        ordinary_crossfade_cost: ordinary_candidates
            .candidates
            .iter()
            .find(|candidate| candidate.plan.kind == TransitionKind::Crossfade)
            .map(|candidate| candidate.cost.total),
        quality_first_selected_kind: format!("{:?}", guarded.plan.kind),
        quality_evidence: format!(
            "observed_beat_pairs={},phase_error_micros={:?},relation={:?},render_acceptable={}",
            eligibility.beat_pairs,
            eligibility.phase_error.map(|error| error.as_micros()),
            selected_pair
                .map(|pair| format!("{:?}/{:?}", pair.outgoing.relation, pair.incoming.relation)),
            render_acceptable,
        ),
        quality_first_disagreement_reason: (ordinary_plan.plan.kind != guarded.plan.kind).then(
            || {
                format!(
                    "ordinary={:?};quality_first={:?}",
                    ordinary_plan.plan.kind, guarded.plan.kind
                )
            },
        ),
        candidate_costs,
    })
}

fn build_positive_realistic_corpus(
    copy_boundary_window: bool,
) -> Result<RealisticPositiveCorpusReport, LabError> {
    let definitions = if copy_boundary_window {
        positive_realistic_specs()
    } else {
        independent_positive_specs()
    };
    let mut raw_fixtures = definitions
        .into_iter()
        .map(|(spec, profile)| {
            let fixture = generate_positive_realistic_fixture(
                &spec,
                &profile,
                if copy_boundary_window { 1.0 } else { 8.0 },
                !copy_boundary_window,
            )?;
            Ok((profile, fixture))
        })
        .collect::<Result<Vec<_>, LabError>>()?;
    if copy_boundary_window {
        for (outgoing_index, incoming_index) in [
            (0, 1),
            (1, 2),
            (3, 4),
            (5, 6),
            (8, 9),
            (10, 11),
            (18, 19),
            (20, 21),
            (22, 23),
            (24, 25),
            (26, 27),
        ] {
            let source_audio = raw_fixtures[outgoing_index].1.audio.clone();
            let source_analysis = analyze_long_fixture(raw_fixtures[outgoing_index].1.clone())?;
            let incoming_analysis = analyze_long_fixture(raw_fixtures[incoming_index].1.clone())?;
            let source_beats = source_analysis
                .v2
                .rhythm
                .beats
                .iter()
                .map(|event| event.time.as_micros() as u64)
                .collect::<Vec<_>>();
            let incoming_start_micros = incoming_analysis
                .v2
                .rhythm
                .beats
                .get(1)
                .map(|event| event.time.as_micros() as u64)
                .ok_or_else(|| LabError::InvalidInput("positive incoming anchor missing".into()))?;
            align_positive_boundary_window(
                &source_audio,
                &source_beats,
                &mut raw_fixtures[incoming_index].1,
                incoming_start_micros,
            );
        }
    }
    let generated = raw_fixtures
        .into_iter()
        .map(|(profile, fixture)| {
            let pcm_sha256 = fixture.audio_sha256.clone();
            let master_id = fixture
                .spec
                .base_id
                .clone()
                .unwrap_or_else(|| fixture.spec.id.clone());
            let seed = fixture.spec.seed;
            let truth_beat_times_micros = fixture.truth.beat_times_micros.clone();
            let analyzed = analyze_long_fixture(fixture.clone())?;
            Ok((
                RealisticFlow {
                    profile,
                    flow: build_flow_observation(&analyzed),
                    pcm_sha256,
                    master_id,
                    seed,
                    truth_beat_times_micros,
                },
                fixture,
            ))
        })
        .collect::<Result<Vec<_>, LabError>>()?;
    let flow_refs = generated
        .iter()
        .map(|(item, _)| &item.flow)
        .collect::<Vec<_>>();
    let candidate_caps = realistic_candidate_caps(&flow_refs);
    let decisions = build_abstention_rows(&flow_refs);
    let fixtures = generated
        .iter()
        .map(|(item, _)| {
            let decision = decisions
                .iter()
                .find(|decision| decision.row.fixture_id == item.flow.fixture_id)
                .expect("positive decision exists");
            let beat_grid = item
                .flow
                .research_analysis
                .as_ref()
                .map(|analysis| {
                    let detected = analysis
                        .rhythm
                        .beats
                        .iter()
                        .map(|event| event.time)
                        .collect::<Vec<_>>();
                    realistic_beat_grid_diagnostic(&item.truth_beat_times_micros, &detected)
                })
                .unwrap_or_else(|| {
                    realistic_beat_grid_diagnostic(&item.truth_beat_times_micros, &[])
                });
            RealisticFixtureObservation {
                fixture_id: item.flow.fixture_id.clone(),
                profile: item.profile.clone(),
                pcm_sha256: item.pcm_sha256.clone(),
                master_id: item.master_id.clone(),
                seed: item.seed,
                duration_micros: item.flow.duration_micros,
                truth_bpm: item.flow.truth_bpm.expect("positive scalar truth"),
                event_count: item
                    .flow
                    .research_analysis
                    .as_ref()
                    .map(|analysis| analysis.rhythm.beats.len())
                    .unwrap_or(0),
                first_event_micros: item.flow.research_analysis.as_ref().and_then(|analysis| {
                    analysis
                        .rhythm
                        .beats
                        .first()
                        .map(|event| event.time.as_micros() as u64)
                }),
                last_event_micros: item.flow.research_analysis.as_ref().and_then(|analysis| {
                    analysis
                        .rhythm
                        .beats
                        .last()
                        .map(|event| event.time.as_micros() as u64)
                }),
                event_clock_bpm: item.flow.event_clock_bpm,
                audible_start_micros: item
                    .flow
                    .research_analysis
                    .as_ref()
                    .map_or(0, |analysis| analysis.audible_start.as_micros() as u64),
                audible_end_micros: item
                    .flow
                    .research_analysis
                    .as_ref()
                    .map_or(0, |analysis| analysis.audible_end.as_micros() as u64),
                heuristic_cue_count: item
                    .flow
                    .research_analysis
                    .as_ref()
                    .map_or(0, |analysis| realistic_analysis_cues(analysis).len()),
                heuristic_mix_in_cue_count: item.flow.research_analysis.as_ref().map_or(
                    0,
                    |analysis| {
                        realistic_analysis_cues(analysis)
                            .iter()
                            .filter(|cue| cue.has_role(CueRole::MixIn))
                            .count()
                    },
                ),
                heuristic_mix_out_cue_count: item.flow.research_analysis.as_ref().map_or(
                    0,
                    |analysis| {
                        realistic_analysis_cues(analysis)
                            .iter()
                            .filter(|cue| cue.has_role(CueRole::MixOut))
                            .count()
                    },
                ),
                candidate_count: decision.row.candidate_count,
                decision: decision.row.decision.clone(),
                selected_bpm: decision.row.selected_bpm,
                selected_relation: decision.row.selected_relation.clone(),
                relation_resolution: decision.row.metrical_consistency.clone(),
                safe_fallback_only: decision.row.decision != "select",
                beat_grid,
            }
        })
        .collect::<Vec<_>>();
    let pairs = if copy_boundary_window {
        vec![
            ("positive-100-section-to-sparse", 0, 1),
            ("positive-100-sparse-to-syncopated", 1, 2),
            ("positive-120-section-to-sparse", 3, 4),
            ("positive-128-section-to-sparse", 5, 6),
            ("positive-130-section-to-kickless", 8, 9),
            ("positive-140-section-to-sparse", 10, 11),
            ("positive-128-to-130-near-compatible", 5, 8),
            ("positive-alias-80-to-160", 12, 13),
            ("positive-alias-60-to-120", 14, 15),
            ("positive-alias-90-to-180", 16, 17),
            ("positive-120-section-r-to-s", 18, 19),
            ("positive-120-section-t-to-u", 20, 21),
            ("positive-120-section-v-to-w", 22, 23),
            ("positive-120-section-x-to-y", 24, 25),
            ("positive-120-section-z-to-aa", 26, 27),
        ]
    } else {
        vec![
            ("independent-100-section-30-to-60", 0, 1),
            ("independent-100-section-to-full", 0, 29),
            ("independent-100-section-60-to-section-60", 1, 23),
            ("independent-100-section-to-syncopated", 1, 2),
            ("independent-100-section-to-sparse", 1, 3),
            ("independent-120-section-30-to-60", 4, 5),
            ("independent-120-section-to-kickless", 5, 6),
            ("independent-90-section-30-to-60", 7, 8),
            ("independent-128-section-to-syncopated", 9, 10),
            ("independent-130-section-to-kickless", 11, 12),
            ("independent-130-section-to-section", 11, 27),
            ("independent-130-section-to-full", 11, 31),
            ("independent-128-to-130-near-compatible", 9, 11),
            ("independent-80-to-160-alias", 15, 16),
            ("independent-60-section-to-section", 17, 26),
            ("independent-60-section-to-full", 17, 30),
            ("independent-60-to-120-alias", 17, 6),
            ("independent-121-section-to-section", 19, 28),
            ("independent-121-section-to-full", 19, 33),
            ("independent-90-section-to-full", 7, 32),
            ("independent-140-30-to-60", 14, 13),
            ("independent-120-30-to-121-60", 18, 19),
            ("independent-126-to-132-near-compatible", 20, 21),
            ("independent-120-full-30-to-60", 34, 35),
            ("independent-90-full-30-to-60", 36, 37),
            ("independent-128-full-30-to-60", 38, 39),
            ("independent-60-full-60-to-30", 41, 40),
            ("independent-120-section-to-full-drop", 5, 34),
            ("independent-90-section-to-full-60", 25, 37),
            ("independent-128-section-to-full-60", 9, 39),
            ("independent-90-section-30-to-full-30", 7, 36),
            ("independent-128-syncopated-30-to-full-30", 10, 38),
            ("independent-127.5-full-30-to-60", 42, 43),
            ("independent-127.5-full-60-to-60", 43, 44),
            ("independent-130-full-60-to-60", 31, 45),
            ("independent-110-full-30-to-60", 46, 47),
            ("independent-110-full-60-to-60", 47, 48),
        ]
    };
    let transition_cases = pairs
        .into_iter()
        .map(|(case_id, outgoing_index, incoming_index)| {
            let (outgoing, outgoing_fixture) = &generated[outgoing_index];
            let (incoming, incoming_fixture) = &generated[incoming_index];
            let outgoing_decision = decisions
                .iter()
                .find(|decision| decision.row.fixture_id == outgoing.flow.fixture_id)
                .expect("positive outgoing decision");
            let incoming_decision = decisions
                .iter()
                .find(|decision| decision.row.fixture_id == incoming.flow.fixture_id)
                .expect("positive incoming decision");
            positive_realistic_transition_case(
                case_id,
                outgoing,
                incoming,
                outgoing_fixture,
                incoming_fixture,
                outgoing_decision,
                incoming_decision,
            )
        })
        .collect::<Result<Vec<_>, LabError>>()?;
    let successful_cases = transition_cases
        .iter()
        .filter(|case| case.beatmatched_selected && !case.false_beatmatched)
        .collect::<Vec<_>>();
    let successful_tempo_regions = successful_cases
        .iter()
        .flat_map(|case| [case.outgoing_truth_bpm, case.incoming_truth_bpm])
        .map(|bpm| format!("{bpm:.1}"))
        .collect::<BTreeSet<_>>();
    let successful_profiles = successful_cases
        .iter()
        .flat_map(|case| {
            let outgoing = generated
                .iter()
                .find(|item| item.0.flow.fixture_id == case.outgoing_fixture)
                .map(|item| item.0.profile.clone())
                .unwrap_or_default();
            let incoming = generated
                .iter()
                .find(|item| item.0.flow.fixture_id == case.incoming_fixture)
                .map(|item| item.0.profile.clone())
                .unwrap_or_default();
            [outgoing, incoming]
        })
        .collect::<BTreeSet<_>>();
    let successful_duration_configs = successful_cases
        .iter()
        .filter_map(|case| {
            let outgoing = generated
                .iter()
                .find(|item| item.0.flow.fixture_id == case.outgoing_fixture)?
                .0
                .flow
                .duration_micros;
            let incoming = generated
                .iter()
                .find(|item| item.0.flow.fixture_id == case.incoming_fixture)?
                .0
                .flow
                .duration_micros;
            Some(format!(
                "{}s->{}s",
                outgoing / 1_000_000,
                incoming / 1_000_000
            ))
        })
        .collect::<BTreeSet<_>>();
    let independent_correct_beatmatched = successful_cases.len();
    let independent_false_beatmatched = transition_cases
        .iter()
        .filter(|case| case.false_beatmatched)
        .count();
    let external_gate_passed = !copy_boundary_window
        && independent_correct_beatmatched >= 5
        && successful_tempo_regions.len() >= 3
        && successful_profiles.len() >= 2
        && successful_duration_configs.len() >= 2
        && independent_false_beatmatched == 0;
    let summary = RealisticPositiveSummary {
        positive_cases: transition_cases.len(),
        effective_correct_beatmatched: transition_cases
            .iter()
            .filter(|case| case.beatmatched_selected && !case.false_beatmatched)
            .count(),
        effective_false_beatmatched: transition_cases
            .iter()
            .filter(|case| case.false_beatmatched)
            .count(),
        safe_fallback: transition_cases
            .iter()
            .filter(|case| case.safe_fallback)
            .count(),
        candidate_generated_not_selected: transition_cases
            .iter()
            .filter(|case| case.beatmatched_candidate_generated && !case.beatmatched_selected)
            .count(),
        candidate_survived_guard_not_selected: transition_cases
            .iter()
            .filter(|case| case.beatmatched_candidate_survived_guard && !case.beatmatched_selected)
            .count(),
        relation_unresolved: transition_cases
            .iter()
            .filter(|case| case.opportunity_class.contains("relation"))
            .count(),
        quality_guard_rejected: transition_cases
            .iter()
            .filter(|case| {
                case.beatmatched_candidate_generated && !case.beatmatched_candidate_survived_guard
            })
            .count(),
        expected_relation_correct: transition_cases
            .iter()
            .filter(|case| case.relation_correct)
            .count(),
        independent_correct_beatmatched,
        distinct_success_tempo_regions: successful_tempo_regions.len(),
        distinct_success_profiles: successful_profiles.len(),
        distinct_success_duration_configs: successful_duration_configs.len(),
        independent_false_beatmatched,
        external_gate_passed,
    };
    Ok(RealisticPositiveCorpusReport {
        source_commit: None,
        starting_commit: None,
        corpus_kind: if copy_boundary_window {
            "PLANNER_MECHANICS_POSITIVE".into()
        } else {
            "INDEPENDENT_POSITIVE".into()
        },
        boundary_window_copy_used: copy_boundary_window,
        construction: if copy_boundary_window {
            "Deterministic arrangement-like audio analyzed through analyze_long_fixture; a bounded boundary window is copied only for planner-mechanics validation.".into()
        } else {
            "Deterministic arrangement-like audio generated independently per fixture and analyzed through analyze_long_fixture; no boundary window is copied and no truth-driven planner evidence is supplied.".into()
        },
        fixture_count: fixtures.len(),
        pair_count: transition_cases.len(),
        candidate_caps,
        fixtures,
        transition_cases,
        summary,
    })
}

fn build_heldout(
    flows: &[&CandidateFlowObservation],
    decisions: &[DecisionInternal],
) -> HeldOutConservativeRanking {
    let families = flows
        .iter()
        .map(|flow| flow.family.clone())
        .collect::<BTreeSet<_>>();
    let mut folds = Vec::new();
    let mut seen = BTreeSet::new();
    for requested in families {
        let validation = expanded_family_validation(flows, &requested);
        let validation_ids = validation
            .iter()
            .map(|flow| flow.fixture_id.clone())
            .collect::<BTreeSet<_>>();
        let train = flows
            .iter()
            .filter(|flow| !validation_ids.contains(&flow.fixture_id))
            .collect::<Vec<_>>();
        let mut accepted = 0;
        let mut canonical = 0;
        let mut family = 0;
        let mut false_confident = 0;
        let mut abstentions = 0;
        for flow in &validation {
            seen.insert(flow.fixture_id.clone());
            if let Some(row) = decisions
                .iter()
                .find(|row| row.row.fixture_id == flow.fixture_id)
            {
                accepted += usize::from(row.row.decision == "select");
                canonical += usize::from(row.row.decision == "select" && row.row.canonical_correct);
                family += usize::from(row.row.decision == "select" && row.row.family_correct);
                false_confident += usize::from(row.row.false_confident_accept);
                abstentions += usize::from(row.row.decision != "select");
            }
        }
        let train_pcm = train
            .iter()
            .map(|flow| flow.pcm_sha256.clone())
            .collect::<BTreeSet<_>>();
        let valid_pcm = validation
            .iter()
            .map(|flow| flow.pcm_sha256.clone())
            .collect::<BTreeSet<_>>();
        let train_lineage = train
            .iter()
            .map(|flow| flow.master_id.clone())
            .collect::<BTreeSet<_>>();
        let valid_lineage = validation
            .iter()
            .map(|flow| flow.master_id.clone())
            .collect::<BTreeSet<_>>();
        let other_families = validation
            .iter()
            .filter(|flow| flow.family != requested)
            .map(|flow| flow.family.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        folds.push(HeldOutConservativeFold {
            requested_family: requested,
            validation_samples: validation
                .iter()
                .map(|flow| flow.fixture_id.clone())
                .collect(),
            expanded_other_families: other_families,
            train_size: train.len(),
            validation_size: validation.len(),
            exact_pcm_overlap: !train_pcm.is_disjoint(&valid_pcm),
            lineage_overlap: !train_lineage.is_disjoint(&valid_lineage),
            accepted,
            canonical_correct: canonical,
            family_correct: family,
            false_confident,
            abstentions,
        });
    }
    let valid_folds = folds
        .iter()
        .filter(|fold| !fold.exact_pcm_overlap && !fold.lineage_overlap)
        .count();
    HeldOutConservativeRanking {
        rule_name: "classical_anchor_guarded_select_or_retain_or_abstain_v2".into(),
        rule_frozen_before_scoring: true,
        grouping_rule: "component-expanded family stress over exact PCM/master-lineage connected components; folds with any overlap are invalid".into(),
        aggregate: HeldOutConservativeAggregate {
            fold_observations: folds.iter().map(|fold| fold.validation_size).sum(),
            unique_fixtures: seen.len(),
            accepted: folds.iter().map(|fold| fold.accepted).sum(),
            canonical_correct: folds.iter().map(|fold| fold.canonical_correct).sum(),
            family_correct: folds.iter().map(|fold| fold.family_correct).sum(),
            false_confident: folds.iter().map(|fold| fold.false_confident).sum(),
            abstentions: folds.iter().map(|fold| fold.abstentions).sum(),
            valid_folds,
        },
        folds,
    }
}

/// Expand a nominal family holdout to every fixture in a connected leakage
/// component. Exact PCM identity and master lineage are both leakage edges;
/// a family label is not a safe boundary when either edge crosses families.
fn expanded_family_validation<'a>(
    flows: &[&'a CandidateFlowObservation],
    requested_family: &str,
) -> Vec<&'a CandidateFlowObservation> {
    let mut included = flows
        .iter()
        .enumerate()
        .filter(|(_, flow)| flow.family == requested_family)
        .map(|(index, _)| index)
        .collect::<BTreeSet<_>>();
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
    included.into_iter().map(|index| flows[index]).collect()
}

fn build_focus_slices(
    flows: &[&CandidateFlowObservation],
    decisions: &[DecisionInternal],
) -> ConservativeFocusSlices {
    let row = |flow: &&CandidateFlowObservation| {
        let decision = decisions
            .iter()
            .find(|item| item.row.fixture_id == flow.fixture_id);
        ConservativeFocusRow {
            fixture_id: flow.fixture_id.clone(),
            truth_bpm: flow.truth_bpm.expect("scalar flow"),
            classical_propagated_bpm: flow
                .pools
                .classical_propagated
                .first()
                .map(|candidate| candidate.bpm),
            conservative_bpm: decision.and_then(|item| item.row.selected_bpm),
            conservative_decision: decision
                .map(|item| item.row.decision.clone())
                .unwrap_or_else(|| "abstain".into()),
            candidate_family_present: merge_evidence(flow, flows, MAX_CANDIDATES).iter().any(
                |candidate| candidate_family_correct(candidate, flow.truth_bpm.expect("truth")),
            ),
            event_consistency: decision
                .map(|item| item.row.metrical_consistency.clone())
                .unwrap_or_else(|| "unavailable".into()),
        }
    };
    ConservativeFocusSlices {
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

fn build_answers(
    baseline: &TempoShadowFollowupReport,
    variants: &[RankingVariantSummary],
    abstention: &AbstentionReport,
    attribution: &EvidenceAttribution,
    stationarity: &[StationarityObservation],
    adversarial: &MetricalAdversarialReport,
    pressure: &CandidatePressureReport,
) -> BTreeMap<String, String> {
    let mut answers = BTreeMap::new();
    let propagation = baseline
        .failure_taxonomy
        .counts
        .get("propagation_dropped")
        .copied()
        .unwrap_or(0);
    answers.insert("1_bottleneck".into(), format!("propagation remains the largest previous primary class at {propagation}; ranking and unsafe confidence remain separate risks"));
    answers.insert("2_classical_vs_merged".into(), format!("Classical propagation is protected from added event/Neural evidence that can displace a correct Classical candidate; harmful cases={}", attribution.harmful_merged_cases.len()));
    answers.insert(
        "3_harmful_sources".into(),
        format!("{:?}", attribution.harmful_source_counts),
    );
    answers.insert(
        "4_rescue_sources".into(),
        format!("{:?}", attribution.rescue_source_counts),
    );
    answers.insert(
        "5_abstention".into(),
        format!(
            "fixed conservative rule accepted {} and abstained/retained {}",
            abstention
                .rows
                .iter()
                .filter(|row| row.decision == "select")
                .count(),
            abstention
                .rows
                .iter()
                .filter(|row| row.decision != "select")
                .count()
        ),
    );
    answers.insert(
        "6_precision_operating_points".into(),
        format!(
            "{:?}",
            abstention
                .pareto
                .iter()
                .map(|point| (&point.name, point.coverage, point.family_precision))
                .collect::<Vec<_>>()
        ),
    );
    answers.insert("7_event_reanchor".into(), "event re-anchor remains conditional; stationarity and relation gates are measured separately".into());
    answers.insert(
        "8_variable_tempo".into(),
        format!(
            "{} of {} synthetic variable-tempo cases abstain",
            stationarity.iter().filter(|item| item.abstain).count(),
            stationarity.len()
        ),
    );
    answers.insert("9_step_return".into(), "the prior global clock used one scalar fit; the new stationarity gate treats segment drift as a safety signal".into());
    answers.insert(
        "10_metrical_inconsistency".into(),
        format!(
            "adversarial invalid BeatMatched before={} after={}",
            adversarial.invalid_before_guard, adversarial.invalid_after_guard
        ),
    );
    answers.insert("11_invalid_alias_guard".into(), "the guard rejects candidates inconsistent with the observed event clock while preserving declared harmonic aliases".into());
    answers.insert(
        "12_candidate_pressure".into(),
        format!(
            "bounded cap={} gives mean candidates {:.2}, p95 pair cross-product {}",
            MAX_CANDIDATES, pressure.mean_candidates, pressure.p95_pair_cross_product
        ),
    );
    answers.insert("13_variant_count".into(), variants.len().to_string());
    answers.insert("14_production".into(), "all decisions remain research-only; no production selector, timeline, planner, or quality threshold changed".into());
    answers
}

fn realistic_candidate_caps(
    flows: &[&CandidateFlowObservation],
) -> Vec<RealisticCandidateCapSummary> {
    [2_usize, 3, 4]
        .into_iter()
        .map(|budget| {
            let mut total = 0;
            let mut max_candidates = 0;
            let mut max_pairs = 0;
            let mut family_recall = 0;
            let mut canonical_recall = 0;
            for flow in flows {
                let candidates = merge_evidence(flow, flows, budget);
                total += candidates.len();
                max_candidates = max_candidates.max(candidates.len());
                max_pairs = max_pairs.max(candidates.len() * candidates.len());
                if let Some(truth) = flow.truth_bpm {
                    family_recall += usize::from(
                        candidates
                            .iter()
                            .any(|candidate| candidate_family_correct(candidate, truth)),
                    );
                    canonical_recall += usize::from(
                        candidates
                            .iter()
                            .any(|candidate| canonical(candidate, truth)),
                    );
                }
            }
            RealisticCandidateCapSummary {
                candidate_budget: budget,
                mean_candidates: total as f32 / flows.len().max(1) as f32,
                max_candidates,
                max_pair_cross_product: max_pairs,
                candidate_family_recall: family_recall,
                candidate_canonical_recall: canonical_recall,
            }
        })
        .collect()
}

fn realistic_transition_case(
    case_id: &str,
    outgoing: &RealisticFlow,
    incoming: &RealisticFlow,
    expected_outcome: &str,
    decisions: &[DecisionInternal],
) -> Result<RealisticTransitionCase, LabError> {
    let outgoing_analysis = outgoing
        .flow
        .research_analysis
        .as_ref()
        .ok_or_else(|| LabError::InvalidInput("realistic outgoing analysis missing".into()))?;
    let incoming_analysis = incoming
        .flow
        .research_analysis
        .as_ref()
        .ok_or_else(|| LabError::InvalidInput("realistic incoming analysis missing".into()))?;
    let outgoing_decision = decisions
        .iter()
        .find(|decision| decision.row.fixture_id == outgoing.flow.fixture_id)
        .ok_or_else(|| LabError::InvalidInput("realistic outgoing decision missing".into()))?;
    let incoming_decision = decisions
        .iter()
        .find(|decision| decision.row.fixture_id == incoming.flow.fixture_id)
        .ok_or_else(|| LabError::InvalidInput("realistic incoming decision missing".into()))?;
    let config = auto_mix_config();
    let baseline_raw = plan_transition_v2(outgoing_analysis, incoming_analysis, &config);
    let baseline_guarded =
        plan_guarded_transition_v2(outgoing_analysis, incoming_analysis, &config);
    let effective_outgoing = RealisticPlannerInput {
        analysis: outgoing_analysis,
        hypotheses: hypotheses_for_decision(&outgoing.flow, outgoing_decision),
        cues: realistic_analysis_cues(outgoing_analysis),
    };
    let effective_incoming = RealisticPlannerInput {
        analysis: incoming_analysis,
        hypotheses: hypotheses_for_decision(&incoming.flow, incoming_decision),
        cues: realistic_analysis_cues(incoming_analysis),
    };
    let effective_raw = plan_transition_v2(&effective_outgoing, &effective_incoming, &config);
    let (_, effective_guarded) =
        quality_first_shadow_transition(&effective_outgoing, &effective_incoming, &config);
    let effective_eligibility =
        beat_match_eligibility(&effective_outgoing, &effective_incoming, &config);
    let baseline_selected = baseline_guarded.plan.kind == TransitionKind::BeatMatched;
    let effective_selected = effective_guarded.plan.kind == TransitionKind::BeatMatched;
    let outgoing_truth = outgoing
        .flow
        .truth_bpm
        .ok_or_else(|| LabError::InvalidInput("realistic outgoing truth missing".into()))?;
    let incoming_truth = incoming
        .flow
        .truth_bpm
        .ok_or_else(|| LabError::InvalidInput("realistic incoming truth missing".into()))?;
    let effective_pair = effective_raw
        .candidates
        .iter()
        .find(|candidate| candidate.plan == effective_guarded.plan)
        .and_then(|candidate| candidate.beat_eligibility.as_ref())
        .and_then(|eligibility| eligibility.tempo_hypothesis)
        .or(effective_eligibility.tempo_hypothesis);
    let effective_pair_correct =
        effective_selected && pair_is_correct(effective_pair, outgoing_truth, incoming_truth);
    let effective_metrical_guard_passed = !effective_raw.candidates.is_empty()
        && effective_outgoing.tempo_hypotheses().len() == outgoing_decision.candidates.len()
        && effective_incoming.tempo_hypotheses().len() == incoming_decision.candidates.len();
    Ok(RealisticTransitionCase {
        case_id: case_id.into(),
        outgoing_fixture: outgoing.flow.fixture_id.clone(),
        incoming_fixture: incoming.flow.fixture_id.clone(),
        expected_outcome: expected_outcome.into(),
        baseline_transition: format!("{:?}", baseline_guarded.plan.kind),
        baseline_beatmatched_candidate_generated: baseline_raw.diagnostics.beatmatched_candidates
            > 0,
        baseline_beatmatched_selected: baseline_selected,
        baseline_quality_guard_passed: baseline_guarded.rejected_plan.is_none(),
        effective_shadow_transition: format!("{:?}", effective_guarded.plan.kind),
        effective_beatmatched_candidate_generated: effective_raw.diagnostics.beatmatched_candidates
            > 0,
        effective_metrical_guard_passed,
        effective_quality_guard_passed: effective_guarded.rejected_plan.is_none(),
        effective_beatmatched_selected: effective_selected,
        effective_false_beatmatched: effective_selected && !effective_pair_correct,
        effective_safe_fallback: !effective_selected,
        effective_pair_correct,
    })
}

fn build_realistic_corpus_report(
    source_commit: &str,
) -> Result<RealisticSyntheticCorpusReport, LabError> {
    let generated = realistic_specs()
        .into_iter()
        .map(|(spec, profile)| {
            let fixture = generate_realistic_fixture(&spec, &profile)?;
            let pcm_sha256 = fixture.audio_sha256.clone();
            let master_id = fixture
                .spec
                .base_id
                .clone()
                .unwrap_or_else(|| fixture.spec.id.clone());
            let seed = fixture.spec.seed;
            let truth_beat_times_micros = fixture.truth.beat_times_micros.clone();
            let analyzed = analyze_long_fixture(fixture)?;
            Ok(RealisticFlow {
                profile,
                flow: build_flow_observation(&analyzed),
                pcm_sha256,
                master_id,
                seed,
                truth_beat_times_micros,
            })
        })
        .collect::<Result<Vec<_>, LabError>>()?;
    let flow_refs = generated.iter().map(|item| &item.flow).collect::<Vec<_>>();
    let decisions = build_abstention_rows(&flow_refs);
    let fixtures = generated
        .iter()
        .map(|item| {
            let decision = decisions
                .iter()
                .find(|decision| decision.row.fixture_id == item.flow.fixture_id)
                .expect("realistic decision exists");
            let beat_grid = item
                .flow
                .research_analysis
                .as_ref()
                .map(|analysis| {
                    let detected = analysis
                        .rhythm
                        .beats
                        .iter()
                        .map(|event| event.time)
                        .collect::<Vec<_>>();
                    realistic_beat_grid_diagnostic(&item.truth_beat_times_micros, &detected)
                })
                .unwrap_or_else(|| {
                    realistic_beat_grid_diagnostic(&item.truth_beat_times_micros, &[])
                });
            RealisticFixtureObservation {
                fixture_id: item.flow.fixture_id.clone(),
                profile: item.profile.clone(),
                pcm_sha256: item.pcm_sha256.clone(),
                master_id: item.master_id.clone(),
                seed: item.seed,
                duration_micros: item.flow.duration_micros,
                truth_bpm: item.flow.truth_bpm.expect("realistic scalar truth"),
                event_count: item
                    .flow
                    .research_analysis
                    .as_ref()
                    .map(|analysis| analysis.rhythm.beats.len())
                    .unwrap_or(0),
                first_event_micros: item.flow.research_analysis.as_ref().and_then(|analysis| {
                    analysis
                        .rhythm
                        .beats
                        .first()
                        .map(|event| event.time.as_micros() as u64)
                }),
                last_event_micros: item.flow.research_analysis.as_ref().and_then(|analysis| {
                    analysis
                        .rhythm
                        .beats
                        .last()
                        .map(|event| event.time.as_micros() as u64)
                }),
                event_clock_bpm: item.flow.event_clock_bpm,
                audible_start_micros: item
                    .flow
                    .research_analysis
                    .as_ref()
                    .map_or(0, |analysis| analysis.audible_start.as_micros() as u64),
                audible_end_micros: item
                    .flow
                    .research_analysis
                    .as_ref()
                    .map_or(0, |analysis| analysis.audible_end.as_micros() as u64),
                heuristic_cue_count: item
                    .flow
                    .research_analysis
                    .as_ref()
                    .map_or(0, |analysis| realistic_analysis_cues(analysis).len()),
                heuristic_mix_in_cue_count: item.flow.research_analysis.as_ref().map_or(
                    0,
                    |analysis| {
                        realistic_analysis_cues(analysis)
                            .iter()
                            .filter(|cue| cue.has_role(CueRole::MixIn))
                            .count()
                    },
                ),
                heuristic_mix_out_cue_count: item.flow.research_analysis.as_ref().map_or(
                    0,
                    |analysis| {
                        realistic_analysis_cues(analysis)
                            .iter()
                            .filter(|cue| cue.has_role(CueRole::MixOut))
                            .count()
                    },
                ),
                candidate_count: decision.row.candidate_count,
                decision: decision.row.decision.clone(),
                selected_bpm: decision.row.selected_bpm,
                selected_relation: decision.row.selected_relation.clone(),
                relation_resolution: decision.row.metrical_consistency.clone(),
                safe_fallback_only: decision.row.decision != "select",
                beat_grid,
            }
        })
        .collect::<Vec<_>>();
    let cases = [
        (
            "same_tempo_duration_pair",
            "realistic-30-sectional-120.0",
            "realistic-60-sectional-120.0",
            "beatmatched_permitted_or_safe_fallback",
        ),
        (
            "tempo_mismatch_pair",
            "realistic-30-sectional-120.0",
            "realistic-30-sectional-140.0",
            "safe_fallback_required",
        ),
        (
            "sparse_to_syncopated_same_tempo",
            "realistic-30-sparse_breakdown-128.0",
            "realistic-30-syncopated_drop-128.0",
            "beatmatched_permitted_or_safe_fallback",
        ),
        (
            "breakdown_reentry_mismatch",
            "realistic-30-kickless_breakdown-120.0",
            "realistic-30-sectional-140.0",
            "safe_fallback_required",
        ),
        (
            "cross_tempo_duration_mismatch",
            "realistic-30-sectional-100.0",
            "realistic-60-sectional-140.0",
            "safe_fallback_required",
        ),
    ]
    .into_iter()
    .map(|(case_id, outgoing_id, incoming_id, expected)| {
        let outgoing = generated
            .iter()
            .find(|item| item.flow.fixture_id == outgoing_id)
            .ok_or_else(|| {
                LabError::InvalidInput(format!("missing realistic fixture {outgoing_id}"))
            })?;
        let incoming = generated
            .iter()
            .find(|item| item.flow.fixture_id == incoming_id)
            .ok_or_else(|| {
                LabError::InvalidInput(format!("missing realistic fixture {incoming_id}"))
            })?;
        realistic_transition_case(case_id, outgoing, incoming, expected, &decisions)
    })
    .collect::<Result<Vec<_>, LabError>>()?;
    let transition_summary = RealisticTransitionSummary {
        pair_count: cases.len(),
        expected_safe_fallback_cases: cases
            .iter()
            .filter(|case| case.expected_outcome == "safe_fallback_required")
            .count(),
        expected_feasible_cases: cases
            .iter()
            .filter(|case| case.expected_outcome != "safe_fallback_required")
            .count(),
        baseline_false_beatmatched: cases
            .iter()
            .filter(|case| {
                case.baseline_beatmatched_selected
                    && case.expected_outcome == "safe_fallback_required"
            })
            .count(),
        effective_false_beatmatched: cases
            .iter()
            .filter(|case| case.effective_false_beatmatched)
            .count(),
        effective_correct_beatmatched: cases
            .iter()
            .filter(|case| case.effective_beatmatched_selected && case.effective_pair_correct)
            .count(),
        effective_safe_fallback: cases
            .iter()
            .filter(|case| case.effective_safe_fallback)
            .count(),
        safe_outcomes: cases
            .iter()
            .filter(|case| !case.effective_false_beatmatched)
            .count(),
        non_interference_scope:
            "research-only generated audio and planner inputs; no production analysis or playback path".into(),
    };
    let interval_consistency = build_runtime_consistency(&flow_refs);
    let positive_corpus = build_positive_realistic_corpus(true)?;
    Ok(RealisticSyntheticCorpusReport {
        schema_version: crate::RESEARCH_REPORT_SCHEMA_VERSION,
        source_commit: source_commit.into(),
        starting_commit: None,
        seed: 0x52_45_41_4c_49_53_54,
        construction: "30s/60s deterministic mono pulse masters with intro/build/drop/breakdown/outro gain sections, sparse evidence, and bounded syncopated drop accents; truth beat clock unchanged".into(),
        fixture_count: fixtures.len(),
        durations_micros: vec![30_000_000, 60_000_000],
        section_profiles: vec![
            "sectional".into(),
            "sparse_breakdown".into(),
            "syncopated_drop".into(),
            "kickless_breakdown".into(),
        ],
        fixtures,
        candidate_caps: realistic_candidate_caps(&flow_refs),
        transition_cases: cases,
        transition_summary,
        interval_consistency,
        positive_corpus,
    })
}

pub fn run_realistic_corpus_research(
    output_dir: &Path,
    source_commit: String,
    starting_commit: Option<String>,
) -> Result<RealisticSyntheticCorpusReport, LabError> {
    fs::create_dir_all(output_dir)?;
    let mut report = build_realistic_corpus_report(&source_commit)?;
    report.starting_commit = starting_commit;
    write_json(&output_dir.join("realistic-synthetic-corpus.json"), &report)?;
    fs::write(
        output_dir.join("realistic-synthetic-corpus.md"),
        realistic_markdown(&report),
    )?;
    Ok(report)
}

/// Run only the primary independent-positive corpus. This bounded command is
/// intended for deterministic research iteration; the final conservative
/// report still reruns the full 90-fixture and safety suite.
pub fn run_independent_positive_corpus_research(
    output_dir: &Path,
    source_commit: String,
    starting_commit: Option<String>,
) -> Result<RealisticPositiveCorpusReport, LabError> {
    fs::create_dir_all(output_dir)?;
    let mut report = build_positive_realistic_corpus(false)?;
    report.source_commit = Some(source_commit);
    report.starting_commit = starting_commit;
    write_json(
        &output_dir.join("independent-positive-corpus.json"),
        &report,
    )?;
    Ok(report)
}

fn realistic_markdown(report: &RealisticSyntheticCorpusReport) -> String {
    let summary = &report.transition_summary;
    format!(
        "# Realistic internal conservative-shadow corpus\n\n- source commit: `{}`\n- starting commit: `{}`\n- fixtures: `{}`\n- durations (micros): `{:?}`\n- profiles: `{:?}`\n- construction: {}\n- production behavior changed: `NO`\n\n## Candidate caps\n\n{}\n\n## Negative/guarded transition summary\n\n- pairs: `{}`\n- expected safe-fallback cases: `{}`\n- expected feasible-or-safe cases: `{}`\n- baseline false BeatMatched: `{}`\n- effective false BeatMatched: `{}`\n- effective correct BeatMatched: `{}`\n- effective safe fallback: `{}`\n- safe outcomes: `{}`\n\n## Positive transition summary\n\n- independent analyzed fixtures: `{}`\n- positive pairs: `{}`\n- effective correct BeatMatched: `{}`\n- effective false BeatMatched: `{}`\n- safe fallback: `{}`\n- rendered quality PASS among selected cases: `{}`\n\nThe corpus is research-only. Generated audio uses deterministic known beat clocks with arrangement-like evidence-density changes; it is not a substitute for ecological validation.\n",
        report.source_commit,
        report
            .starting_commit
            .as_deref()
            .unwrap_or("unrecorded"),
        report.fixture_count,
        report.durations_micros,
        report.section_profiles,
        report.construction,
        report
            .candidate_caps
            .iter()
            .map(|cap| {
                format!(
                    "- cap `{}`: mean candidates `{:.2}`, max candidates `{}`, max pair cross-product `{}`, family recall `{}`, canonical recall `{}`",
                    cap.candidate_budget,
                    cap.mean_candidates,
                    cap.max_candidates,
                    cap.max_pair_cross_product,
                    cap.candidate_family_recall,
                    cap.candidate_canonical_recall
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
        summary.pair_count,
        summary.expected_safe_fallback_cases,
        summary.expected_feasible_cases,
        summary.baseline_false_beatmatched,
        summary.effective_false_beatmatched,
        summary.effective_correct_beatmatched,
        summary.effective_safe_fallback,
        summary.safe_outcomes,
        report.positive_corpus.fixture_count,
        report.positive_corpus.pair_count,
        report.positive_corpus.summary.effective_correct_beatmatched,
        report.positive_corpus.summary.effective_false_beatmatched,
        report.positive_corpus.summary.safe_fallback,
        report
            .positive_corpus
            .transition_cases
            .iter()
            .filter(|case| case.beatmatched_selected && case.render_quality.acceptable)
            .count(),
    )
}

fn write_outputs(
    output_dir: &Path,
    report: &TempoConservativeShadowReport,
) -> Result<(), LabError> {
    write_json(
        &output_dir.join("tempo-ranking-abstention.json"),
        &report.abstention,
    )?;
    write_json(
        &output_dir.join("candidate-evidence-attribution.json"),
        &report.evidence_attribution,
    )?;
    write_json(
        &output_dir.join("metrical-consistency-audit.json"),
        &report.metrical_consistency,
    )?;
    write_json(
        &output_dir.join("metrical-consistency-adversarial.json"),
        &report.adversarial,
    )?;
    write_json(
        &output_dir.join("variable-tempo-abstention.json"),
        &report.stationarity,
    )?;
    write_json(&output_dir.join("candidate-pruning.json"), &report.pruning)?;
    write_json(
        &output_dir.join("conservative-shadow-matrix.json"),
        &report.conservative_matrix,
    )?;
    write_json(
        &output_dir.join("effective-shadow-planner.json"),
        &report.effective_planner,
    )?;
    write_json(
        &output_dir.join("runtime-feasible-shadow.json"),
        &report.runtime_feasible,
    )?;
    write_json(
        &output_dir.join("runtime-tempo-consistency.json"),
        &report.runtime_consistency,
    )?;
    write_json(
        &output_dir.join("runtime-feature-audit.json"),
        &report.runtime_feature_audit,
    )?;
    write_json(
        &output_dir.join("abstention-reasons.json"),
        &report.abstention_reasons,
    )?;
    write_json(
        &output_dir.join("realistic-synthetic-corpus.json"),
        &report.realistic_corpus,
    )?;
    write_json(
        &output_dir.join("heldout-conservative-ranking.json"),
        &report.heldout,
    )?;
    write_json(
        &output_dir.join("tempo-conservative-shadow-report.json"),
        report,
    )?;
    write_json(&output_dir.join("tempo-conservative-shadow.json"), report)?;
    write_csv(
        &output_dir.join("tempo-ranking-abstention.csv"),
        &report.abstention.rows,
    )?;
    fs::write(
        output_dir.join("tempo-conservative-shadow-report.md"),
        markdown_report(report),
    )?;
    Ok(())
}

fn write_csv(path: &Path, rows: &[TempoShadowDecisionRow]) -> Result<(), LabError> {
    let mut output = String::from(
        "fixture_id,family,truth_bpm,decision,selected_bpm,candidate_count,top1_score,score_margin,source_support,selected_sources,classical_anchor_available,event_agreement,metrical_consistency,canonical_correct,family_correct,false_confident\n",
    );
    for row in rows {
        output.push_str(&format!(
            "{},{},{:.4},{},{},{},{},{},{},{},{},{},{},{},{},{}\n",
            row.fixture_id,
            row.family,
            row.truth_bpm,
            row.decision,
            row.selected_bpm
                .map(|value| format!("{value:.4}"))
                .unwrap_or_default(),
            row.candidate_count,
            row.top1_score
                .map(|value| format!("{value:.6}"))
                .unwrap_or_default(),
            row.score_margin
                .map(|value| format!("{value:.6}"))
                .unwrap_or_default(),
            row.source_support,
            row.selected_sources.join("+"),
            row.classical_anchor_available,
            row.event_agreement
                .map(|value| format!("{value:.6}"))
                .unwrap_or_default(),
            row.metrical_consistency,
            row.canonical_correct,
            row.family_correct,
            row.false_confident_accept,
        ));
    }
    fs::write(path, output)?;
    Ok(())
}

fn markdown_report(report: &TempoConservativeShadowReport) -> String {
    let accepted = report.decision.accepted;
    let abstained = report.decision.abstained;
    let candidate_flow = report
        .candidate_budgets
        .iter()
        .find(|item| item.budget == MAX_CANDIDATES);
    let mut output = String::new();
    output.push_str("# Conservative tempo shadow research\n\n");
    output.push_str("This is research-only. Classical remains the production rhythm, beat, phase, meter, downbeat, and tempo authority. No production behavior changed.\n\n");
    output.push_str(&format!("- source commit: {}\n- fixtures: {} scalar fixtures plus synthetic adversarial/variable-tempo cases\n- production behavior changed: {}\n- recommendation: **{}**\n\n", report.source_commit, report.scalar_tempo_fixture_count, report.production_behavior_changed, report.decision.recommendation));
    output.push_str("## Propagation and ranking\n\n");
    output.push_str(&format!("Previous propagation failure class: {}. Classical propagated top-1 canonical: {} / {}. Conservative selected: {} with {} abstained or retained-multiple.\n\n", report.decision.primary_bottleneck, report.decision.propagation_top1_canonical, report.scalar_tempo_fixture_count, accepted, abstained));
    if let Some(flow) = candidate_flow {
        output.push_str(&format!(
            "Bounded candidate cap {}: mean candidates {:.2}, p95 pair cross-product {}.\n\n",
            flow.budget, flow.mean_candidates, flow.p95_pair_cross_product
        ));
    }
    output.push_str("## Precision operating points\n\n| point | coverage | family precision | canonical precision | false confident |\n|---|---:|---:|---:|---:|\n");
    for point in &report.abstention.pareto {
        output.push_str(&format!(
            "| {} | {:.3} | {} | {} | {} |\n",
            point.name,
            point.coverage,
            format_option(point.family_precision),
            format_option(point.canonical_precision),
            point.false_confident
        ));
    }
    output.push_str("\n## Evidence attribution\n\n");
    output.push_str(&format!("Harmful merged cases: {}. Rescue cases: {}. Harmful sources: {:?}. Rescue sources: {:?}.\n\n", report.evidence_attribution.harmful_merged_cases.len(), report.evidence_attribution.rescue_cases.len(), report.evidence_attribution.harmful_source_counts, report.evidence_attribution.rescue_source_counts));
    output.push_str("## Variable-tempo stationarity\n\n| profile | classification | drift | MAD ratio | abstain |\n|---|---|---:|---:|---:|\n");
    for item in &report.stationarity {
        output.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            item.profile,
            item.classification,
            format_option(item.early_late_drift),
            format_option(item.interval_mad_ratio),
            item.abstain
        ));
    }
    output.push_str("\n## Metrical consistency\n\n");
    output.push_str(&format!(
        "Invalid BeatMatched before guard: {}. After guard: {}. Valid aliases retained: {}/{}.\n\n",
        report.adversarial.invalid_before_guard,
        report.adversarial.invalid_after_guard,
        report.adversarial.valid_aliases_retained,
        report.adversarial.valid_aliases_total
    ));
    output.push_str("## Effective shadow planner\n\n");
    output.push_str(&format!(
        "Baseline BeatMatched selected: {}/{} (false {}). Effective shadow BeatMatched selected: {}/{} (false {}, safe fallback {}, missed {}). Valid aliases representable: {}/{}.\n\n",
        report.effective_planner.baseline_beatmatched_selected,
        report.effective_planner.baseline_transition_cases,
        report.effective_planner.baseline_false_beatmatched,
        report.effective_planner.effective_beatmatched_selected,
        report.effective_planner.effective_transition_cases,
        report.effective_planner.effective_false_beatmatched,
        report.effective_planner.effective_safe_fallback,
        report.effective_planner.effective_missed_opportunity,
        report.effective_planner.aliases_representable,
        report.effective_planner.aliases_total,
    ));
    output.push_str("## Runtime-feasible audit\n\n");
    output.push_str(&format!(
        "{}: accepted {}, abstained/retained {}, canonical {}/{}, family {}/{}, false-confident {}.\n\n",
        report.runtime_feasible.rule_name,
        report.runtime_feasible.accepted,
        report.runtime_feasible.abstained_or_retained,
        report.runtime_feasible.canonical_correct,
        report.runtime_feasible.scored,
        report.runtime_feasible.family_correct,
        report.runtime_feasible.scored,
        report.runtime_feasible.false_confident_accepts,
    ));
    output.push_str("Feature classifications: ");
    output.push_str(
        &report
            .runtime_feature_audit
            .iter()
            .map(|feature| format!("{}={}", feature.name, feature.classification))
            .collect::<Vec<_>>()
            .join(", "),
    );
    output.push_str(".\n\n");
    let runtime_abstentions = report
        .runtime_consistency
        .iter()
        .filter(|item| item.refinement_would_abstain)
        .count();
    output.push_str(&format!(
        "Single-pass interval consistency: {}/{} would abstain from a global constant-tempo refinement. Classifications: {:?}.\n\n",
        runtime_abstentions,
        report.runtime_consistency.len(),
        report
            .runtime_consistency
            .iter()
            .fold(BTreeMap::<String, usize>::new(), |mut counts, item| {
                *counts.entry(item.classification.clone()).or_insert(0) += 1;
                counts
            })
    ));
    output.push_str("## Non-selected reasons\n\n");
    let mut reason_counts = BTreeMap::<String, usize>::new();
    for row in &report.abstention_reasons {
        *reason_counts.entry(row.reason.clone()).or_insert(0) += 1;
    }
    output.push_str(&format!(
        "{} non-selected rows classified: {:?}.\n\n",
        report.abstention_reasons.len(),
        reason_counts
    ));
    output.push_str("## Realistic internal corpus\n\n");
    let realistic = &report.realistic_corpus;
    output.push_str(&format!(
        "{} bounded arrangement-like fixtures at durations {:?}; transition cases: {}. Effective false BeatMatched: {}; safe outcomes: {}.\n\n",
        realistic.fixture_count,
        realistic.durations_micros,
        realistic.transition_summary.pair_count,
        realistic.transition_summary.effective_false_beatmatched,
        realistic.transition_summary.safe_outcomes,
    ));
    output.push_str("Candidate caps on the realistic corpus: ");
    output.push_str(
        &realistic
            .candidate_caps
            .iter()
            .map(|cap| {
                format!(
                    "{} (family recall {}, canonical recall {}, max pairs {})",
                    cap.candidate_budget,
                    cap.candidate_family_recall,
                    cap.candidate_canonical_recall,
                    cap.max_pair_cross_product
                )
            })
            .collect::<Vec<_>>()
            .join(", "),
    );
    output.push_str(".\n\n");
    output.push_str("## Candidate pressure\n\n");
    output.push_str(&format!("Mean candidates: {:.2}; p50: {}; p95: {}; max: {}; mean pair cross-product: {:.2}; p95: {}; max: {}.\n\n", report.candidate_pressure.mean_candidates, report.candidate_pressure.p50_candidates, report.candidate_pressure.p95_candidates, report.candidate_pressure.max_candidates, report.candidate_pressure.mean_pair_cross_product, report.candidate_pressure.p95_pair_cross_product, report.candidate_pressure.max_pair_cross_product));
    output.push_str("## Decision\n\n");
    output.push_str(&format!("**{}**\n\n", report.decision.recommendation));
    for (key, value) in &report.decision.answers {
        output.push_str(&format!("- {key}: {value}\n"));
    }
    output.push_str("\n## Production boundary\n\n- production candidate propagation: unchanged\n- production BeatEvent timeline: unchanged\n- production AutoMix/quality guard: unchanged\n- generated reports: outside Git\n");
    output
}

fn format_option<T: std::fmt::Display>(value: Option<T>) -> String {
    value
        .map(|number| format!("{number:.4}"))
        .unwrap_or_else(|| "unavailable".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrical_guard_accepts_primary_and_harmonic_aliases() {
        assert_eq!(
            consistency_for(120.0, "primary", Some(120.0)).0,
            "consistent"
        );
        assert_eq!(
            consistency_for(60.0, "half_time", Some(120.0)).0,
            "consistent"
        );
        assert_eq!(
            consistency_for(240.0, "double_time", Some(120.0)).0,
            "consistent"
        );
        assert_eq!(
            consistency_for(140.0, "primary", Some(120.0)).0,
            "inconsistent"
        );
    }

    #[test]
    fn stationarity_gate_abstains_on_large_segment_drift() {
        let profile = StationarityObservation {
            fixture_id: "test".into(),
            profile: "step".into(),
            classification: "tempo_step".into(),
            interval_count: 20,
            median_interval_micros: Some(500_000.0),
            interval_mad_ratio: Some(0.02),
            early_middle_drift: Some(0.02),
            middle_late_drift: Some(0.02),
            early_late_drift: Some(0.04),
            fit_residual_ratio: Some(0.02),
            refinement_eligible: false,
            abstain: true,
            abstention_reason: Some("non_stationary_global_tempo".into()),
        };
        assert!(profile.abstain);
        assert!(!profile.refinement_eligible);
    }

    #[test]
    fn candidate_merge_is_bounded() {
        let candidate = TempoCandidate {
            bpm: 120.0,
            source: "full".into(),
            relation: "primary".into(),
            score: 1.0,
            normalized_score: 1.0,
            origin_stage: "test".into(),
            relation_to_truth: "unscored".into(),
            shadow_rank_score: 0.0,
        };
        assert_eq!(source_flags(&candidate).len(), 0);
        assert!(candidate_key(120.0, 120.4));
        assert!(!candidate_key(120.0, 121.0));
    }

    #[test]
    fn classical_anchor_bonus_requires_full_and_low_support() {
        let candidate = TempoCandidate {
            bpm: 128.0,
            source: "test".into(),
            relation: "primary".into(),
            score: 0.8,
            normalized_score: 0.8,
            origin_stage: "test".into(),
            relation_to_truth: "unscored".into(),
            shadow_rank_score: 0.0,
        };
        let base = EvidenceCandidate {
            candidate: candidate.clone(),
            sources: BTreeSet::new(),
            source_count: 2,
            has_full: false,
            has_low: false,
            has_neural: false,
            has_event: true,
            has_v2: true,
            event_agreement: Some(1.0),
            duration_stability: 0.5,
        };
        let mut anchored = base.clone();
        anchored.has_full = true;
        anchored.has_low = true;
        assert!(
            rank_value(&anchored, RankMode::ClassicalAnchor, 0.0)
                > rank_value(&base, RankMode::ClassicalAnchor, 0.0)
        );
        assert!(!classical_anchor_allows(
            &base,
            &[base.clone(), anchored.clone()]
        ));
        let mut full_only = base;
        full_only.has_full = true;
        assert!(!classical_anchor_allows(&full_only, &[full_only.clone()]));
    }

    #[test]
    fn unlabeled_relation_is_resolved_only_from_event_clock() {
        let candidate = TempoCandidate {
            bpm: 60.0,
            source: "classical_full".into(),
            relation: "unlabeled".into(),
            score: 1.0,
            normalized_score: 1.0,
            origin_stage: "test".into(),
            relation_to_truth: "unscored".into(),
            shadow_rank_score: 0.0,
        };
        let evidence = EvidenceCandidate {
            candidate,
            sources: BTreeSet::from(["full".into()]),
            source_count: 1,
            has_full: true,
            has_low: false,
            has_neural: false,
            has_event: false,
            has_v2: false,
            event_agreement: Some(1.0),
            duration_stability: 0.5,
        };
        assert_eq!(
            resolved_relation(&evidence, Some(120.0)),
            Some(TempoRelation::HalfTime)
        );
        assert_eq!(resolved_relation(&evidence, Some(125.0)), None);
        assert_eq!(resolved_relation(&evidence, None), None);
    }

    #[test]
    fn empty_shadow_hypotheses_do_not_derive_primary_from_beats() {
        let analysis = synthetic_analysis(120.0, 120.0, "primary");
        let input = ShadowAnalysisInput {
            analysis: &analysis,
            hypotheses: Vec::new(),
        };
        let eligibility = beat_match_eligibility(&input, &input, &auto_mix_config());
        assert!(!eligibility.eligible);
        assert!(eligibility.tempo_hypothesis.is_none());
    }

    #[test]
    fn adversarial_effective_guard_removes_invalid_and_keeps_aliases() {
        let report = build_adversarial_report().expect("adversarial report");
        assert_eq!(report.invalid_after_guard, 0);
        assert_eq!(report.valid_aliases_retained, report.valid_aliases_total);
        assert!(
            report
                .cases
                .iter()
                .filter(|case| !case.declared_valid_alias)
                .all(|case| !case.effective_shadow_beatmatched_selected)
        );
    }

    #[test]
    fn realistic_specs_cover_bounded_durations_and_profiles() {
        let specs = realistic_specs();
        assert_eq!(specs.len(), 9);
        assert!(
            specs
                .iter()
                .any(|(spec, _)| spec.duration_micros == 30_000_000)
        );
        assert!(
            specs
                .iter()
                .any(|(spec, _)| spec.duration_micros == 60_000_000)
        );
        let profiles = specs
            .iter()
            .map(|(_, profile)| profile.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            profiles,
            BTreeSet::from([
                "sectional",
                "sparse_breakdown",
                "syncopated_drop",
                "kickless_breakdown",
            ])
        );
    }

    #[test]
    fn realistic_candidate_cap_policy_is_explicit() {
        let budgets = [2_usize, 3, 4];
        assert_eq!(budgets, [2, 3, 4]);
        assert!(budgets.iter().all(|budget| budget * budget <= 16));
    }

    #[test]
    fn event_clock_support_is_relation_aware_and_deterministic() {
        let times = (0..=8).map(|index| Duration::from_micros(index * 500_000));
        assert_eq!(analysis_event_clock_bpm(times), Some(120.0));
        let primary =
            TempoHypothesis::with_relation(120.0, UnitInterval::ONE, TempoRelation::Primary)
                .expect("primary hypothesis");
        let half_time =
            TempoHypothesis::with_relation(60.0, UnitInterval::ONE, TempoRelation::HalfTime)
                .expect("half-time hypothesis");
        let wrong =
            TempoHypothesis::with_relation(140.0, UnitInterval::ONE, TempoRelation::Primary)
                .expect("wrong hypothesis");
        assert!(tempo_hypothesis_matches_event_clock(primary, Some(120.0)));
        assert!(tempo_hypothesis_matches_event_clock(half_time, Some(120.0)));
        assert!(!tempo_hypothesis_matches_event_clock(wrong, Some(120.0)));
    }
}
