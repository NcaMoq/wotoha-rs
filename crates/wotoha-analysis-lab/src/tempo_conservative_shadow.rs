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
        BeatEvent, Confidence, MeterHypothesis, ModelScore, Support, TempoHypothesis,
        TempoRelation, TrackAnalysisV2, UnitInterval,
    },
    automix::{
        AutoMixConfig, TransitionKind, beat_match_eligibility, plan_guarded_transition_v2,
        plan_transition_v2,
    },
};

use super::{
    tempo_ambiguity_research::{AnalyzedFixture, analyze_long_fixture, long_spec},
    tempo_shadow_followup::{
        CandidateFlowObservation, TempoCandidate, TempoShadowFollowupReport,
        run_tempo_shadow_followup,
    },
};
use crate::{EventStyle, FixtureFamily, LabError, TempoProfile, generate_fixture, write_json};

const RELATIVE_TOLERANCE: f32 = 0.005;
const MAX_CANDIDATES: usize = 4;
const ACCEPT_SCORE: f32 = 0.68;
const ACCEPT_MARGIN: f32 = 0.08;
const ACCEPT_EVENT_AGREEMENT: f32 = 0.80;
const STATIONARY_DRIFT: f64 = 0.01;
const STATIONARY_DISPERSION: f64 = 0.01;

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
    pub heldout: HeldOutConservativeRanking,
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
    pub false_confident_accepts: usize,
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

#[derive(Clone, Copy)]
enum RankMode {
    Evidence,
    Consensus,
    Margin,
    EventGated,
}

#[derive(Clone, Debug)]
struct DecisionInternal {
    row: TempoShadowDecisionRow,
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
        rule_name: "fixed_conservative_select_or_retain_or_abstain_v1".into(),
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
    let decision = ConservativeDecision {
        recommendation: "KEEP RESEARCH ONLY".into(),
        primary_bottleneck: "propagation_and_unsafe_ranking".into(),
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
        heldout,
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
    match mode {
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
            let mut candidate_family_recall = 0;
            let mut candidate_canonical_recall = 0;
            for flow in flows {
                let truth = flow.truth_bpm.expect("scalar flow");
                if name == "current_v2" {
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
                false_confident_accepts: false_confident,
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
            let candidates = rank_candidates(flow, flows, MAX_CANDIDATES, RankMode::EventGated);
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
            });
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
                    first
                        .as_ref()
                        .expect("safe candidate")
                        .candidate
                        .relation
                        .clone()
                }),
                candidate_count: candidates.len(),
                top1_score: first.as_ref().map(|item| item.candidate.shadow_rank_score),
                top2_score: second.map(|item| item.candidate.shadow_rank_score),
                score_margin: margin,
                source_support,
                event_agreement,
                duration_stability: first
                    .as_ref()
                    .map(|item| item.duration_stability)
                    .unwrap_or(0.0),
                metrical_consistency: first
                    .as_ref()
                    .map(|item| {
                        consistency_for(
                            item.candidate.bpm,
                            &item.candidate.relation,
                            flow.event_clock_bpm,
                        )
                        .0
                    })
                    .unwrap_or_else(|| "unavailable".into()),
                canonical_correct: safe && canonical_correct,
                family_correct: safe && family_correct,
                false_confident_accept: safe && !family_correct,
                correct_abstention: !safe && !canonical_correct,
            };
            DecisionInternal { row }
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
                let candidates = rank_candidates(flow, flows, MAX_CANDIDATES, RankMode::EventGated);
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
                        .is_some_and(|value| value >= ACCEPT_EVENT_AGREEMENT);
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
        let quality_selected = guarded.plan.kind == TransitionKind::BeatMatched;
        let invalid_before = quality_selected && !declared_valid_alias;
        let metrical_survives = consistency != "inconsistent";
        let invalid_after = invalid_before && metrical_survives;
        output.push(MetricalAdversarialCase {
            case_id: case_id.into(),
            event_clock_bpm: event_bpm,
            candidate_bpm,
            relation: relation.into(),
            declared_valid_alias,
            consistency,
            planner_physical_eligible: eligibility.eligible,
            beatmatched_generated: planned.diagnostics.beatmatched_candidates > 0,
            quality_guard_selected: quality_selected,
            guarded_beatmatched: quality_selected,
            beatmatched_after_metrical_guard: quality_selected && metrical_survives,
            invalid_before_guard: invalid_before,
            invalid_after_guard: invalid_after,
            transition: format!("{:?}", guarded.plan.kind),
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
        .filter(|case| case.declared_valid_alias && case.consistency == "consistent")
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
        let outgoing_decision = outgoing.and_then(|flow| {
            decisions
                .iter()
                .find(|row| row.row.fixture_id == flow.fixture_id)
        });
        let incoming_decision = incoming.and_then(|flow| {
            decisions
                .iter()
                .find(|row| row.row.fixture_id == flow.fixture_id)
        });
        variants.push(ConservativeVariantCase {
            pair_id: case.pair_id.clone(),
            variant: if case.variant == "merged_shadow" {
                "conservative_shadow"
            } else {
                "current_v2"
            }
            .into(),
            selected_bpm_outgoing: outgoing_decision.and_then(|row| row.row.selected_bpm),
            selected_bpm_incoming: incoming_decision.and_then(|row| row.row.selected_bpm),
            decision_outgoing: outgoing_decision
                .map(|row| row.row.decision.clone())
                .unwrap_or_else(|| "abstain".into()),
            decision_incoming: incoming_decision
                .map(|row| row.row.decision.clone())
                .unwrap_or_else(|| "abstain".into()),
            metrical_guard_outgoing: outgoing_decision
                .map(|row| row.row.metrical_consistency.clone())
                .unwrap_or_else(|| "unavailable".into()),
            metrical_guard_incoming: incoming_decision
                .map(|row| row.row.metrical_consistency.clone())
                .unwrap_or_else(|| "unavailable".into()),
            beatmatched_generated: case.beatmatched_candidate_generated,
            beatmatched_selected: case.beatmatched_selected,
            correct_beatmatched: case.correct_beatmatched,
            false_beatmatched: case.false_beatmatched,
            safe_fallback: case.safe_fallback,
            missed_opportunity: case.missed_opportunity,
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
            .filter(|case| {
                case.correct_beatmatched
                    && case.decision_outgoing == "select"
                    && case.decision_incoming == "select"
            })
            .count(),
        conservative_false_beatmatched: shadow_cases
            .iter()
            .filter(|case| {
                case.false_beatmatched
                    && case.decision_outgoing == "select"
                    && case.decision_incoming == "select"
            })
            .count(),
        safe_fallback: shadow_cases
            .iter()
            .filter(|case| {
                case.safe_fallback
                    || case.decision_outgoing != "select"
                    || case.decision_incoming != "select"
            })
            .count(),
        missed_opportunity: shadow_cases
            .iter()
            .filter(|case| case.missed_opportunity)
            .count(),
        variants,
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
        let validation = flows
            .iter()
            .filter(|flow| flow.family == requested)
            .collect::<Vec<_>>();
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
        rule_name: "fixed_conservative_select_or_retain_or_abstain_v1".into(),
        rule_frozen_before_scoring: true,
        grouping_rule: "component-expanded family stress; this implementation uses family exclusion and reports exact PCM/master overlap explicitly".into(),
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
        "fixture_id,family,truth_bpm,decision,selected_bpm,candidate_count,top1_score,score_margin,source_support,event_agreement,metrical_consistency,canonical_correct,family_correct,false_confident\n",
    );
    for row in rows {
        output.push_str(&format!(
            "{},{},{:.4},{},{},{},{},{},{},{},{},{},{},{}\n",
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
    output.push_str("## Candidate pressure\n\n");
    output.push_str(&format!("Mean candidates: {:.2}; p50: {}; p95: {}; max: {}; mean pair cross-product: {:.2}; p95: {}; max: {}.\n\n", report.candidate_pressure.mean_candidates, report.candidate_pressure.p50_candidates, report.candidate_pressure.p95_candidates, report.candidate_pressure.max_candidates, report.candidate_pressure.mean_pair_cross_product, report.candidate_pressure.p95_pair_cross_product, report.candidate_pressure.max_pair_cross_product));
    output.push_str("## Decision\n\n");
    output.push_str("**KEEP RESEARCH ONLY**\n\n");
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
}
