//! Timeline-first AutoMix V2 candidate generation and selection.

use std::time::Duration;

use super::diagnostics::{AutoMixV2Reason, CandidateRejection, PlannerDiagnostics};
use super::reliability::{
    BeatTimeline, TimelineEvent, compute_pair_reliability, pair_reliability,
    reliability_for_analysis, reliability_for_timeline, reliability_for_timeline_window,
    timeline_from_analysis, timeline_from_track_analysis_v2,
};
use super::scoring::{TransitionCostBreakdown, rhythm_uncertainty_cost};
use super::{
    AutoMixConfig, AutoMixQualityIssue, AutoMixQualityReport, TempoEnvelope, TrackAnalysis,
    TransitionKind, TransitionPlan, evaluate_transition_quality_with_base_gains,
    harmonic_compatibility, plan_transition_timing,
};

const MIN_V2_BEAT_PAIRS: usize = 3;
const MAX_CUE_TRANSITION_CANDIDATES: usize = 64;
const MIN_CUE_OVERLAP: Duration = Duration::from_secs(1);
const BLEND_BEAT_FAMILY: [usize; 4] = [8, 16, 32, 64];
const STRUCTURE_CUE_PROXIMITY_WEIGHT: f32 = 0.10;
const STRUCTURE_HANDOFF_ALIGNMENT_WEIGHT: f32 = 0.08;
const STRUCTURE_BOUNDARY_CROSSING_WEIGHT: f32 = 0.04;
const STRUCTURE_PERIODIC_PRIOR_WEIGHT: f32 = 0.15;
const MAX_STRUCTURE_ALIGNMENT_COST: f32 = 0.05;

use crate::analysis::{
    CueRole, DjCue, PhraseBoundary, TempoRelation, TrackAnalysisV2, UnitInterval,
    bound_cue_candidates, top_mix_in_cues, top_mix_out_cues,
};
use crate::config::BeatmatchBlendConfig;

/// Adapter boundary accepted by the V2 planner.  Keeping this trait generic
/// lets existing V1 callers use the new cost/diagnostic API while versioned
/// callers retain the richer `TrackAnalysisV2` beat-event timeline.
pub trait V2AnalysisInput {
    fn as_v2_legacy_view(&self) -> TrackAnalysis;

    /// Beat-indexed cue evidence. Legacy adapters intentionally have no cues.
    fn cue_candidates(&self) -> Vec<DjCue> {
        Vec::new()
    }

    /// Phrase boundaries used to keep cue starts within the first/last phrase.
    fn phrase_boundaries(&self) -> Vec<PhraseBoundary> {
        Vec::new()
    }

    fn rhythm_timeline(&self) -> BeatTimeline {
        timeline_from_analysis(&self.as_v2_legacy_view())
    }

    fn tempo_hypotheses(&self) -> Vec<TempoHypothesis> {
        tempo_hypotheses(&self.as_v2_legacy_view())
    }
}

impl V2AnalysisInput for TrackAnalysis {
    fn as_v2_legacy_view(&self) -> TrackAnalysis {
        self.clone()
    }
}

impl V2AnalysisInput for TrackAnalysisV2 {
    fn as_v2_legacy_view(&self) -> TrackAnalysis {
        let mut view = TrackAnalysis::unanalyzed(self.duration);
        view.audible_start = self.audible_start;
        view.audible_end = self.audible_end;
        // Copy observed event timestamps exactly.  This is an adapter for the
        // legacy quality evaluator; no event is generated from a tempo.
        view.beat_markers = self.rhythm.beats.iter().map(|event| event.time).collect();
        view.beat_marker_confidences = self
            .rhythm
            .beats
            .iter()
            .map(|event| {
                event
                    .beat_model_score
                    .map(|score| score.get())
                    .unwrap_or_else(|| event.confidence().get())
            })
            .collect();
        view.first_beat = view.beat_markers.first().copied();
        view.bpm = self.rhythm.primary_tempo();
        view.beat_confidence = self
            .rhythm
            .beats
            .iter()
            .map(|event| event.confidence().get())
            .sum::<f32>()
            / self.rhythm.beats.len().max(1) as f32;

        // The legacy BeatGrid is explicitly 4/4.  Only adapt a resolved
        // 4-beat meter into its phase-facing fields; an unresolved meter (or
        // a resolved meter the legacy representation cannot express) remains
        // auxiliary V2 evidence and must not masquerade as confirmed phase.
        if let Some(meter) = self
            .rhythm
            .resolved_meter_hypothesis()
            .filter(|meter| meter.beats_per_bar == 4)
        {
            view.downbeat_confidence = meter.score.get();
            view.first_downbeat = self
                .rhythm
                .beats
                .get(usize::from(meter.downbeat_phase))
                .map(|event| event.time);
        }

        let beat_times = &view.beat_markers;
        if let Some(section) = self
            .structure
            .sections
            .iter()
            .find(|section| section.has_label(crate::analysis::SectionLabel::Intro))
        {
            view.intro_end = section_end_time(section.end_beat, beat_times, self.audible_end);
            view.intro_confidence = section.boundary_confidence.get();
        }
        if let Some(section) = self
            .structure
            .sections
            .iter()
            .find(|section| section.has_label(crate::analysis::SectionLabel::Outro))
        {
            view.outro_start = section_start_time(section.start_beat, beat_times, self.audible_end);
            view.outro_confidence = section.boundary_confidence.get();
        }

        if self.vocal.validate() && !self.vocal.activity.is_empty() {
            view.vocal_activity = self
                .vocal
                .activity
                .iter()
                .map(|value| quantize_unit_interval(value.get()))
                .collect();
            view.vocal_activity_confidences = self
                .vocal
                .confidences
                .iter()
                .map(|value| quantize_unit_interval(value.get()))
                .collect();
            view.vocal_activity_rate = self.vocal.rate_hz.min(u16::from(u8::MAX)) as u8;
        }

        if self.energy.validate() && !self.energy.profile.is_empty() {
            view.energy_profile = self
                .energy
                .profile
                .iter()
                .map(|value| quantize_unit_interval(value.get()))
                .collect();
            view.energy_profile_rate = self.energy.rate_hz.min(u16::from(u8::MAX)) as u8;
        }
        view.rms_dbfs = self.energy.rms_dbfs;
        view.sample_peak_dbfs = self.energy.sample_peak_dbfs;
        view.integrated_lufs = self.energy.integrated_lufs;
        view.true_peak_dbtp = self.energy.true_peak_dbtp;

        view.musical_key = self.tonal.global_key.and_then(|key| {
            key.validate().then_some(crate::automix::MusicalKey {
                tonic: key.tonic,
                mode: match key.mode {
                    crate::analysis::KeyMode::Major => crate::automix::KeyMode::Major,
                    crate::analysis::KeyMode::Minor => crate::automix::KeyMode::Minor,
                },
                confidence: key.confidence.get(),
            })
        });
        view
    }

    fn rhythm_timeline(&self) -> BeatTimeline {
        timeline_from_track_analysis_v2(self)
    }

    fn cue_candidates(&self) -> Vec<DjCue> {
        self.cues.clone()
    }

    fn phrase_boundaries(&self) -> Vec<PhraseBoundary> {
        self.structure.phrase_boundaries.clone()
    }

    fn tempo_hypotheses(&self) -> Vec<TempoHypothesis> {
        if !self.rhythm.tempo_hypotheses.is_empty() {
            return self.rhythm.tempo_hypotheses.clone();
        }
        // A manually assembled V2 fixture may contain only observed beat
        // events.  Derive one tempo *hypothesis* from their intervals when
        // the persisted list is empty; this does not synthesize or alter any
        // BeatEvent timeline entries.
        self.rhythm
            .primary_tempo()
            .and_then(|bpm| {
                TempoHypothesis::with_relation(bpm, UnitInterval::ONE, TempoRelation::Primary)
            })
            .into_iter()
            .collect()
    }
}

fn quantize_unit_interval(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * f32::from(u8::MAX)).round() as u8
}

fn section_start_time(
    start_beat: usize,
    beat_times: &[Duration],
    fallback: Duration,
) -> Option<Duration> {
    Some(beat_times.get(start_beat).copied().unwrap_or(fallback))
}

fn section_end_time(
    end_beat: usize,
    beat_times: &[Duration],
    fallback: Duration,
) -> Option<Duration> {
    Some(beat_times.get(end_beat).copied().unwrap_or(fallback))
}

pub use crate::analysis::TempoHypothesis;

/// A pair from the outgoing/incoming hypothesis cross product.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TempoHypothesisPair {
    pub outgoing: TempoHypothesis,
    pub incoming: TempoHypothesis,
    /// Playback ratio, explicitly `outgoing / incoming`.
    pub ratio: f32,
    pub normalized_adjustment: f32,
    pub weight: f32,
    pub cost: f32,
}

/// Test/debug-visible evidence explaining a cue-backed BeatMatched candidate.
/// Non-cue candidates leave this field empty; their cost and eligibility still
/// expose the strategy and rhythm decisions that were evaluated.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CueCandidateDiagnostics {
    pub outgoing_cue_index: usize,
    pub incoming_cue_index: usize,
    pub cue_score: f32,
    pub cue_suitability_cost: f32,
    pub tempo_pair: TempoHypothesisPair,
    pub rhythm_reliability: f32,
}

impl TempoHypothesisPair {
    pub fn new(outgoing: TempoHypothesis, incoming: TempoHypothesis) -> Option<Self> {
        if !outgoing.validate() || !incoming.validate() {
            return None;
        }
        let ratio = outgoing.bpm / incoming.bpm;
        if !ratio.is_finite() || ratio <= 0.0 {
            return None;
        }
        let weight = (outgoing.relative_weight.get() * incoming.relative_weight.get())
            .sqrt()
            .clamp(0.0, 1.0);
        let normalized_adjustment = (ratio - 1.0).abs();
        let stretch_cost = normalized_adjustment;
        let weight_cost = (1.0 - weight) * TEMPO_PAIR_WEIGHT_COST;
        let relation_penalty =
            tempo_relation_penalty(outgoing.relation) + tempo_relation_penalty(incoming.relation);
        Some(Self {
            outgoing,
            incoming,
            ratio,
            normalized_adjustment,
            weight,
            // Stretch remains the dominant term.  Confidence and interpretation
            // relation are soft preferences, so a strong half/double-time
            // hypothesis can beat a weak primary one without being treated as
            // equivalent to the primary interpretation.
            cost: stretch_cost + weight_cost + relation_penalty,
        })
    }

    /// Stretch contribution to the pair-selection cost.
    pub fn stretch_cost(self) -> f32 {
        self.normalized_adjustment
    }

    /// Joint confidence penalty derived from both hypothesis weights.
    pub fn weight_cost(self) -> f32 {
        (1.0 - self.weight) * TEMPO_PAIR_WEIGHT_COST
    }

    /// Sum of the two relation penalties; primary hypotheses add no cost.
    pub fn relation_penalty(self) -> f32 {
        tempo_relation_penalty(self.outgoing.relation)
            + tempo_relation_penalty(self.incoming.relation)
    }

    pub fn within_adjustment(self, max_tempo_adjustment: f32) -> bool {
        max_tempo_adjustment.is_finite()
            && max_tempo_adjustment >= 0.0
            && self.ratio.is_finite()
            && self.ratio > 0.0
            && (self.ratio - 1.0).abs() <= max_tempo_adjustment
    }
}

const TEMPO_PAIR_WEIGHT_COST: f32 = 0.02;

fn tempo_relation_penalty(relation: TempoRelation) -> f32 {
    match relation {
        TempoRelation::Primary => 0.0,
        TempoRelation::HalfTime | TempoRelation::DoubleTime => 0.001,
        TempoRelation::Alternative => 0.003,
    }
}

pub fn cross_product_tempo_hypotheses(
    outgoing: &[TempoHypothesis],
    incoming: &[TempoHypothesis],
) -> Vec<TempoHypothesisPair> {
    const MAX_HYPOTHESES_PER_TRACK: usize = 8;
    const MAX_TEMPO_COMBINATIONS: usize = MAX_HYPOTHESES_PER_TRACK * MAX_HYPOTHESES_PER_TRACK;
    let outgoing = bounded_tempo_hypotheses(outgoing, MAX_HYPOTHESES_PER_TRACK);
    let incoming = bounded_tempo_hypotheses(incoming, MAX_HYPOTHESES_PER_TRACK);
    let mut pairs = outgoing
        .iter()
        .copied()
        .flat_map(|outgoing| {
            incoming
                .iter()
                .copied()
                .filter_map(move |incoming| TempoHypothesisPair::new(outgoing, incoming))
        })
        .collect::<Vec<_>>();
    pairs.sort_by(|left, right| {
        left.cost
            .total_cmp(&right.cost)
            .then_with(|| {
                left.normalized_adjustment
                    .total_cmp(&right.normalized_adjustment)
            })
            .then_with(|| right.weight.total_cmp(&left.weight))
            .then_with(|| left.outgoing.bpm.total_cmp(&right.outgoing.bpm))
            .then_with(|| left.incoming.bpm.total_cmp(&right.incoming.bpm))
    });
    pairs.truncate(MAX_TEMPO_COMBINATIONS);
    pairs
}

fn bounded_tempo_hypotheses(hypotheses: &[TempoHypothesis], limit: usize) -> Vec<TempoHypothesis> {
    let mut hypotheses = hypotheses
        .iter()
        .copied()
        .filter(TempoHypothesis::validate)
        .collect::<Vec<_>>();
    hypotheses.sort_by(|left, right| {
        right
            .relative_weight
            .get()
            .total_cmp(&left.relative_weight.get())
            .then_with(|| left.bpm.total_cmp(&right.bpm))
            .then_with(|| {
                tempo_relation_penalty(left.relation)
                    .total_cmp(&tempo_relation_penalty(right.relation))
            })
    });
    hypotheses.dedup_by(|left, right| left.bpm == right.bpm && left.relation == right.relation);
    hypotheses.truncate(limit);
    hypotheses
}

pub fn tempo_hypothesis_pairs(
    outgoing: &[TempoHypothesis],
    incoming: &[TempoHypothesis],
) -> Vec<TempoHypothesisPair> {
    cross_product_tempo_hypotheses(outgoing, incoming)
}

/// Return hypotheses from the BPM value already present in analysis.  The
/// default adapter intentionally returns one hypothesis: aliases can be
/// supplied explicitly by a richer analysis caller and are still combined by
/// [`cross_product_tempo_hypotheses`].
pub fn tempo_hypotheses(analysis: &TrackAnalysis) -> Vec<TempoHypothesis> {
    analysis
        .bpm
        .filter(|bpm| bpm.is_finite() && *bpm > 0.0)
        .and_then(|bpm| {
            TempoHypothesis::with_relation(bpm, UnitInterval::ONE, TempoRelation::Primary)
        })
        .map(|hypothesis| vec![hypothesis])
        .unwrap_or_default()
}

/// Alias useful to callers that make half/double-time hypotheses upstream.
pub fn select_tempo_hypothesis_pair(
    outgoing: &[TempoHypothesis],
    incoming: &[TempoHypothesis],
    max_tempo_adjustment: f32,
) -> Option<TempoHypothesisPair> {
    cross_product_tempo_hypotheses(outgoing, incoming)
        .into_iter()
        .filter(|pair| pair.within_adjustment(max_tempo_adjustment))
        .min_by(|left, right| {
            left.cost
                .total_cmp(&right.cost)
                .then_with(|| {
                    left.normalized_adjustment
                        .total_cmp(&right.normalized_adjustment)
                })
                .then_with(|| right.weight.total_cmp(&left.weight))
        })
}

pub type BeatMatchRejection = CandidateRejection;

#[derive(Clone, Debug, PartialEq)]
pub struct BeatMatchEligibility {
    pub eligible: bool,
    pub rejection: Option<BeatMatchRejection>,
    pub outgoing_events: usize,
    pub incoming_events: usize,
    pub beat_pairs: usize,
    pub phase_error: Option<Duration>,
    pub tempo_hypothesis: Option<TempoHypothesisPair>,
}

impl BeatMatchEligibility {
    pub fn eligible(
        outgoing_events: usize,
        incoming_events: usize,
        beat_pairs: usize,
        phase_error: Option<Duration>,
        tempo_hypothesis: TempoHypothesisPair,
    ) -> Self {
        Self {
            eligible: true,
            rejection: None,
            outgoing_events,
            incoming_events,
            beat_pairs,
            phase_error,
            tempo_hypothesis: Some(tempo_hypothesis),
        }
    }

    fn rejected(reason: BeatMatchRejection) -> Self {
        Self {
            eligible: false,
            rejection: Some(reason),
            outgoing_events: 0,
            incoming_events: 0,
            beat_pairs: 0,
            phase_error: None,
            tempo_hypothesis: None,
        }
    }
}

/// A candidate consists of an executable legacy plan plus its V2 cost.
#[derive(Clone, Debug, PartialEq)]
pub struct TransitionCandidate {
    pub plan: TransitionPlan,
    pub cost: TransitionCostBreakdown,
    pub beat_eligibility: Option<BeatMatchEligibility>,
    pub hard_rejection: Option<BeatMatchRejection>,
    pub cue_diagnostics: Option<CueCandidateDiagnostics>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TransitionPlanV2 {
    pub plan: TransitionPlan,
    pub cost: TransitionCostBreakdown,
    pub diagnostics: PlannerDiagnostics,
    pub candidates: Vec<TransitionCandidate>,
}

pub type V2TransitionPlan = TransitionPlanV2;

impl TransitionPlanV2 {
    pub fn kind(&self) -> TransitionKind {
        self.plan.kind
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct V2GuardedTransitionPlan {
    pub plan: TransitionPlan,
    pub cost: TransitionCostBreakdown,
    pub cue_diagnostics: Option<CueCandidateDiagnostics>,
    pub quality: AutoMixQualityReport,
    pub diagnostics: PlannerDiagnostics,
    pub rejected_plan: Option<TransitionPlan>,
    pub rejected_quality: Option<AutoMixQualityReport>,
}

pub type GuardedTransitionPlanV2 = V2GuardedTransitionPlan;

/// Evaluate hard BeatMatched eligibility using observed marker timelines.
/// Global confidence, low-band coverage, model confidence, downbeat confidence
/// and phrase priors are deliberately absent from this gate.
pub fn beat_match_eligibility<O: V2AnalysisInput, I: V2AnalysisInput>(
    outgoing: &O,
    incoming: &I,
    config: &AutoMixConfig,
) -> BeatMatchEligibility {
    let outgoing_timeline = outgoing.rhythm_timeline();
    let incoming_timeline = incoming.rhythm_timeline();
    let outgoing_hypotheses = outgoing.tempo_hypotheses();
    let incoming_hypotheses = incoming.tempo_hypotheses();
    let outgoing = outgoing.as_v2_legacy_view();
    let incoming = incoming.as_v2_legacy_view();
    beat_match_eligibility_with_context(
        &outgoing,
        &incoming,
        config,
        &outgoing_timeline,
        &incoming_timeline,
        &outgoing_hypotheses,
        &incoming_hypotheses,
    )
}

pub fn check_beat_match_eligibility<O: V2AnalysisInput, I: V2AnalysisInput>(
    outgoing: &O,
    incoming: &I,
    config: &AutoMixConfig,
) -> BeatMatchEligibility {
    beat_match_eligibility(outgoing, incoming, config)
}

pub fn beat_match_eligibility_for_timelines(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    outgoing_timeline: &BeatTimeline,
    incoming_timeline: &BeatTimeline,
    config: &AutoMixConfig,
) -> BeatMatchEligibility {
    beat_match_eligibility_with_context(
        outgoing,
        incoming,
        config,
        outgoing_timeline,
        incoming_timeline,
        &tempo_hypotheses(outgoing),
        &tempo_hypotheses(incoming),
    )
}

fn beat_match_eligibility_with_context(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    config: &AutoMixConfig,
    outgoing_timeline: &BeatTimeline,
    incoming_timeline: &BeatTimeline,
    outgoing_hypotheses: &[TempoHypothesis],
    incoming_hypotheses: &[TempoHypothesis],
) -> BeatMatchEligibility {
    if !config.max_tempo_adjustment.is_finite()
        || config.max_tempo_adjustment < 0.0
        || !config.crossfade.is_zero() && !config.crossfade.as_secs_f64().is_finite()
    {
        return BeatMatchEligibility::rejected(BeatMatchRejection::InvalidDspParameters);
    }
    let outgoing_events = usable_events(outgoing, outgoing_timeline);
    let incoming_events = usable_events(incoming, incoming_timeline);
    if !outgoing_timeline.has_strict_times() || !incoming_timeline.has_strict_times() {
        return BeatMatchEligibility::rejected(BeatMatchRejection::InvalidBeatEventTimes);
    }
    if outgoing_events.is_empty() || incoming_events.is_empty() {
        return BeatMatchEligibility::rejected(BeatMatchRejection::NoUsableRhythmTimeline);
    }
    if outgoing_events.len() < MIN_V2_BEAT_PAIRS || incoming_events.len() < MIN_V2_BEAT_PAIRS {
        return BeatMatchEligibility::rejected(BeatMatchRejection::NotEnoughBeatPairs);
    }

    if outgoing_hypotheses.is_empty() || incoming_hypotheses.is_empty() {
        return BeatMatchEligibility::rejected(BeatMatchRejection::NoCompatibleTempoHypothesis);
    }
    let Some(pair) = select_tempo_hypothesis_pair(
        outgoing_hypotheses,
        incoming_hypotheses,
        config.max_tempo_adjustment,
    ) else {
        return BeatMatchEligibility::rejected(BeatMatchRejection::TempoAdjustmentExceeded);
    };
    if !pair.ratio.is_finite() || pair.ratio <= 0.0 {
        return BeatMatchEligibility::rejected(BeatMatchRejection::InvalidDspParameters);
    }
    let Some((outgoing_start, incoming_start, duration)) =
        physical_window(outgoing, incoming, config, incoming_events[0].time)
    else {
        return BeatMatchEligibility::rejected(BeatMatchRejection::PhysicalWindowUnavailable);
    };
    // The incoming deck is mapped through the tempo ratio, so validate the
    // source-time extent of the actual DSP window as well as its output-time
    // extent.  A ratio greater than one can otherwise overrun the audible
    // incoming bound even when the native crossfade fits.
    let Some(mapped_duration) = scale_duration(duration, f64::from(pair.ratio)) else {
        return BeatMatchEligibility::rejected(BeatMatchRejection::InvalidDspParameters);
    };
    let mapped_incoming_end = incoming_start
        .checked_add(mapped_duration)
        .unwrap_or(Duration::MAX);
    if mapped_incoming_end > incoming.audible_end {
        return BeatMatchEligibility::rejected(BeatMatchRejection::PhysicalWindowUnavailable);
    }
    let (pairs, phase_error) = phase_pairs(
        &outgoing_events,
        &incoming_events,
        outgoing_start,
        incoming_start,
        duration,
        pair.ratio,
    );
    if pairs < MIN_V2_BEAT_PAIRS {
        return BeatMatchEligibility::rejected(BeatMatchRejection::NotEnoughBeatPairs);
    }
    if phase_error.is_some_and(|error| !error.is_zero() && !error.as_secs_f64().is_finite()) {
        return BeatMatchEligibility::rejected(BeatMatchRejection::InvalidDspParameters);
    }
    // Phase drift is intentionally not an eligibility gate.  The hard
    // eligibility contract is limited to usable evidence, strict times,
    // enough in-window pairs, compatible tempo/DSP bounds, and a physical
    // window.  A candidate with excessive phase error is retained for the
    // V2 quality guard, which can reject it without conflating that hard
    // quality outcome with missing rhythm evidence.
    BeatMatchEligibility::eligible(
        outgoing_events.len(),
        incoming_events.len(),
        pairs,
        phase_error,
        pair,
    )
}

/// Plan and select among the physically available BeatMatched, Crossfade and
/// Gapless candidates by total cost.
pub fn plan_transition_v2<O: V2AnalysisInput, I: V2AnalysisInput>(
    outgoing: &O,
    incoming: &I,
    config: &AutoMixConfig,
) -> TransitionPlanV2 {
    plan_transition_v2_with_blend_config(
        outgoing,
        incoming,
        config,
        &BeatmatchBlendConfig::default(),
    )
}

pub fn plan_transition_v2_with_blend_config<O: V2AnalysisInput, I: V2AnalysisInput>(
    outgoing: &O,
    incoming: &I,
    config: &AutoMixConfig,
    blend_config: &BeatmatchBlendConfig,
) -> TransitionPlanV2 {
    plan_transition_v2_with_blend_config_and_base_gains(
        outgoing,
        incoming,
        config,
        blend_config,
        1.0,
        1.0,
    )
}

/// Plan V2 candidates using the persistent gains already applied to each
/// runtime deck. The gains affect rendered-quality scoring only; the returned
/// plan continues to carry relative transition gains.
pub fn plan_transition_v2_with_blend_config_and_base_gains<
    O: V2AnalysisInput,
    I: V2AnalysisInput,
>(
    outgoing: &O,
    incoming: &I,
    config: &AutoMixConfig,
    blend_config: &BeatmatchBlendConfig,
    outgoing_base_gain: f32,
    incoming_base_gain: f32,
) -> TransitionPlanV2 {
    let outgoing_timeline = outgoing.rhythm_timeline();
    let incoming_timeline = incoming.rhythm_timeline();
    let outgoing_hypotheses = outgoing.tempo_hypotheses();
    let incoming_hypotheses = incoming.tempo_hypotheses();
    let outgoing_cues = outgoing.cue_candidates();
    let incoming_cues = incoming.cue_candidates();
    let outgoing_phrase_boundaries = outgoing.phrase_boundaries();
    let incoming_phrase_boundaries = incoming.phrase_boundaries();
    let outgoing = outgoing.as_v2_legacy_view();
    let incoming = incoming.as_v2_legacy_view();
    let (plan, _) = plan_transition_v2_internal_with_context(
        &outgoing,
        &incoming,
        config,
        &outgoing_timeline,
        &incoming_timeline,
        &outgoing_hypotheses,
        &incoming_hypotheses,
        &outgoing_cues,
        &incoming_cues,
        &outgoing_phrase_boundaries,
        &incoming_phrase_boundaries,
        blend_config,
        outgoing_base_gain,
        incoming_base_gain,
    );
    plan
}

/// Diagnostics-bearing spelling kept explicit for integrations that prefer a
/// named entry point.  Diagnostics are also present on the returned plan.
pub fn plan_transition_v2_with_diagnostics<O: V2AnalysisInput, I: V2AnalysisInput>(
    outgoing: &O,
    incoming: &I,
    config: &AutoMixConfig,
) -> TransitionPlanV2 {
    plan_transition_v2(outgoing, incoming, config)
}

pub fn plan_transition_v2_diagnostics<O: V2AnalysisInput, I: V2AnalysisInput>(
    outgoing: &O,
    incoming: &I,
    config: &AutoMixConfig,
) -> (TransitionPlanV2, PlannerDiagnostics) {
    let plan = plan_transition_v2(outgoing, incoming, config);
    let diagnostics = plan.diagnostics.clone();
    (plan, diagnostics)
}

pub fn explain_beatmatch_decision_v2<O: V2AnalysisInput, I: V2AnalysisInput>(
    outgoing: &O,
    incoming: &I,
    config: &AutoMixConfig,
) -> AutoMixV2Reason {
    let planned = plan_transition_v2(outgoing, incoming, config);
    if !config.enabled {
        return AutoMixV2Reason::Disabled;
    }
    if planned.plan.kind == TransitionKind::BeatMatched {
        return AutoMixV2Reason::BeatMatchedSelected;
    }
    if planned.diagnostics.eligible_but_crossfaded {
        return AutoMixV2Reason::BeatMatchAvailableButCrossfadePreferred;
    }
    if planned.diagnostics.beatmatched_candidates > 0
        && planned.plan.kind == TransitionKind::Gapless
    {
        return AutoMixV2Reason::BeatMatchAvailableButGaplessPreferred;
    }
    if let Some(rejection) = planned.diagnostics.hard_rejections.first() {
        return rejection.reason();
    }
    match planned.plan.kind {
        TransitionKind::Crossfade => AutoMixV2Reason::CrossfadeSelected,
        TransitionKind::Gapless => AutoMixV2Reason::GaplessSelected,
        TransitionKind::BeatMatched => AutoMixV2Reason::BeatMatchedSelected,
    }
}

/// Production-facing V2 hook for callers that already own the versioned
/// analysis record.  It copies no rhythm events and leaves the input/V1
/// analysis untouched; all beat matching is based on `rhythm.beats`.
pub fn plan_transition_v2_for_analysis(
    outgoing: &TrackAnalysisV2,
    incoming: &TrackAnalysisV2,
    config: &AutoMixConfig,
) -> TransitionPlanV2 {
    plan_transition_v2(outgoing, incoming, config)
}

pub fn plan_transition_v2_for_analysis_with_blend_config(
    outgoing: &TrackAnalysisV2,
    incoming: &TrackAnalysisV2,
    config: &AutoMixConfig,
    blend_config: &BeatmatchBlendConfig,
) -> TransitionPlanV2 {
    plan_transition_v2_with_blend_config(outgoing, incoming, config, blend_config)
}

pub fn plan_guarded_transition_v2<O: V2AnalysisInput, I: V2AnalysisInput>(
    outgoing: &O,
    incoming: &I,
    config: &AutoMixConfig,
) -> V2GuardedTransitionPlan {
    plan_guarded_transition_v2_with_diagnostics(outgoing, incoming, config)
}

pub fn plan_guarded_transition_v2_with_diagnostics<O: V2AnalysisInput, I: V2AnalysisInput>(
    outgoing: &O,
    incoming: &I,
    config: &AutoMixConfig,
) -> V2GuardedTransitionPlan {
    plan_guarded_transition_v2_with_blend_config(
        outgoing,
        incoming,
        config,
        &BeatmatchBlendConfig::default(),
    )
}

pub fn plan_guarded_transition_v2_with_blend_config<O: V2AnalysisInput, I: V2AnalysisInput>(
    outgoing: &O,
    incoming: &I,
    config: &AutoMixConfig,
    blend_config: &BeatmatchBlendConfig,
) -> V2GuardedTransitionPlan {
    plan_guarded_transition_v2_with_blend_config_and_base_gains(
        outgoing,
        incoming,
        config,
        blend_config,
        1.0,
        1.0,
    )
}

/// Guarded V2 planning paired with the base gains used by playback.
pub fn plan_guarded_transition_v2_with_blend_config_and_base_gains<
    O: V2AnalysisInput,
    I: V2AnalysisInput,
>(
    outgoing: &O,
    incoming: &I,
    config: &AutoMixConfig,
    blend_config: &BeatmatchBlendConfig,
    outgoing_base_gain: f32,
    incoming_base_gain: f32,
) -> V2GuardedTransitionPlan {
    let planned = plan_transition_v2_with_blend_config_and_base_gains(
        outgoing,
        incoming,
        config,
        blend_config,
        outgoing_base_gain,
        incoming_base_gain,
    );
    let outgoing = outgoing.as_v2_legacy_view();
    let incoming = incoming.as_v2_legacy_view();
    let raw_quality = evaluate_transition_quality_with_base_gains(
        &outgoing,
        &incoming,
        &planned.plan,
        outgoing_base_gain,
        incoming_base_gain,
    );
    let quality = v2_quality_report(&raw_quality, planned.plan.kind);
    let selected_cue_diagnostics = planned
        .candidates
        .iter()
        .find(|candidate| candidate.plan == planned.plan)
        .and_then(|candidate| candidate.cue_diagnostics);
    if !quality_has_v2_hard_issue(&raw_quality, planned.plan.kind) {
        return V2GuardedTransitionPlan {
            plan: planned.plan,
            cost: planned.cost,
            cue_diagnostics: selected_cue_diagnostics,
            quality,
            diagnostics: planned.diagnostics,
            rejected_plan: None,
            rejected_quality: None,
        };
    }

    // Re-run the bounded candidate list in ascending cost and retain the first
    // candidate passing V2's hard guard.  Soft quality concerns remain costs.
    for candidate in &planned.candidates {
        let raw_candidate_quality = evaluate_transition_quality_with_base_gains(
            &outgoing,
            &incoming,
            &candidate.plan,
            outgoing_base_gain,
            incoming_base_gain,
        );
        if !quality_has_v2_hard_issue(&raw_candidate_quality, candidate.plan.kind) {
            let candidate_quality = v2_quality_report(&raw_candidate_quality, candidate.plan.kind);
            let mut diagnostics = planned.diagnostics.clone();
            diagnostics.add_reason(AutoMixV2Reason::QualityGuardRejected);
            diagnostics.set_selection(
                candidate.plan.kind,
                candidate.cost,
                planned.diagnostics.beatmatched_candidates > 0,
            );
            return V2GuardedTransitionPlan {
                plan: candidate.plan.clone(),
                cost: candidate.cost,
                cue_diagnostics: candidate.cue_diagnostics,
                quality: candidate_quality,
                diagnostics,
                rejected_plan: Some(planned.plan),
                rejected_quality: Some(quality),
            };
        }
    }

    // Gapless is always the terminal safety strategy, even if its quality
    // report contains diagnostics about malformed source metadata.
    let gapless = gapless_plan(&outgoing, &incoming);
    let gapless_quality = v2_quality_report(
        &evaluate_transition_quality_with_base_gains(
            &outgoing,
            &incoming,
            &gapless,
            outgoing_base_gain,
            incoming_base_gain,
        ),
        gapless.kind,
    );
    let mut diagnostics = planned.diagnostics;
    diagnostics.add_reason(AutoMixV2Reason::QualityGuardRejected);
    diagnostics.set_selection(
        TransitionKind::Gapless,
        TransitionCostBreakdown::from_components(
            TransitionKind::Gapless,
            0.0,
            config.max_tempo_adjustment,
            None,
            compute_pair_reliability(&outgoing, &incoming),
            config.min_beat_confidence,
            None,
            0.0,
        ),
        diagnostics.beatmatched_candidates > 0,
    );
    V2GuardedTransitionPlan {
        plan: gapless,
        cost: TransitionCostBreakdown::from_components(
            TransitionKind::Gapless,
            0.0,
            config.max_tempo_adjustment,
            None,
            compute_pair_reliability(&outgoing, &incoming),
            config.min_beat_confidence,
            None,
            0.0,
        ),
        cue_diagnostics: None,
        quality: gapless_quality,
        diagnostics,
        rejected_plan: Some(planned.plan),
        rejected_quality: Some(quality),
    }
}

pub fn plan_guarded_transition_v2_diagnostics<O: V2AnalysisInput, I: V2AnalysisInput>(
    outgoing: &O,
    incoming: &I,
    config: &AutoMixConfig,
) -> (V2GuardedTransitionPlan, PlannerDiagnostics) {
    let plan = plan_guarded_transition_v2(outgoing, incoming, config);
    let diagnostics = plan.diagnostics.clone();
    (plan, diagnostics)
}

/// Guarded production hook paired with [`plan_transition_v2_for_analysis`].
/// The returned quality report is V2-filtered while V1's evaluator and
/// thresholds remain available to legacy callers.
pub fn plan_guarded_transition_v2_for_analysis(
    outgoing: &TrackAnalysisV2,
    incoming: &TrackAnalysisV2,
    config: &AutoMixConfig,
) -> V2GuardedTransitionPlan {
    plan_guarded_transition_v2(outgoing, incoming, config)
}

pub fn plan_guarded_transition_v2_for_analysis_with_blend_config(
    outgoing: &TrackAnalysisV2,
    incoming: &TrackAnalysisV2,
    config: &AutoMixConfig,
    blend_config: &BeatmatchBlendConfig,
) -> V2GuardedTransitionPlan {
    plan_guarded_transition_v2_with_blend_config(outgoing, incoming, config, blend_config)
}

pub fn plan_guarded_transition_v2_for_analysis_with_blend_config_and_base_gains(
    outgoing: &TrackAnalysisV2,
    incoming: &TrackAnalysisV2,
    config: &AutoMixConfig,
    blend_config: &BeatmatchBlendConfig,
    outgoing_base_gain: f32,
    incoming_base_gain: f32,
) -> V2GuardedTransitionPlan {
    plan_guarded_transition_v2_with_blend_config_and_base_gains(
        outgoing,
        incoming,
        config,
        blend_config,
        outgoing_base_gain,
        incoming_base_gain,
    )
}

#[allow(clippy::too_many_arguments)]
fn plan_transition_v2_internal_with_context(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    config: &AutoMixConfig,
    outgoing_timeline: &BeatTimeline,
    incoming_timeline: &BeatTimeline,
    outgoing_hypotheses: &[TempoHypothesis],
    incoming_hypotheses: &[TempoHypothesis],
    outgoing_cue_candidates: &[DjCue],
    incoming_cue_candidates: &[DjCue],
    outgoing_phrase_boundaries: &[PhraseBoundary],
    incoming_phrase_boundaries: &[PhraseBoundary],
    blend_config: &BeatmatchBlendConfig,
    outgoing_base_gain: f32,
    incoming_base_gain: f32,
) -> (TransitionPlanV2, BeatMatchEligibility) {
    let gapless = gapless_plan(outgoing, incoming);
    let pair_reliability = pair_reliability(
        reliability_for_timeline(outgoing_timeline).reliability,
        reliability_for_timeline(incoming_timeline).reliability,
    );
    let mut candidates = Vec::with_capacity(MAX_CUE_TRANSITION_CANDIDATES + 2);
    let eligibility = beat_match_eligibility_with_context(
        outgoing,
        incoming,
        config,
        outgoing_timeline,
        incoming_timeline,
        outgoing_hypotheses,
        incoming_hypotheses,
    );
    let mut diagnostics = PlannerDiagnostics::default();
    if !config.enabled {
        diagnostics.add_reason(AutoMixV2Reason::Disabled);
    }
    let (cue_candidates, cue_counts) = cue_transition_candidates(
        outgoing,
        incoming,
        config,
        outgoing_timeline,
        incoming_timeline,
        outgoing_hypotheses,
        incoming_hypotheses,
        outgoing_cue_candidates,
        incoming_cue_candidates,
        outgoing_phrase_boundaries,
        incoming_phrase_boundaries,
        blend_config,
        outgoing_base_gain,
        incoming_base_gain,
    );
    diagnostics.outgoing_mix_out_cues = cue_counts.0;
    diagnostics.incoming_mix_in_cues = cue_counts.1;
    diagnostics.cue_pairs_checked = cue_counts.2;
    diagnostics.cue_tempo_combinations_checked = cue_counts.3;
    if cue_counts.2 == 0
        && let Some(reason) = eligibility.rejection
    {
        diagnostics.add_reason(reason.reason());
    }
    for candidate in cue_candidates {
        diagnostics.add_candidate(TransitionKind::BeatMatched);
        candidates.push(candidate);
    }

    if candidates
        .iter()
        .all(|candidate| candidate.plan.kind != TransitionKind::BeatMatched)
        && cue_counts.2 == 0
        && config.enabled
        && eligibility.eligible
        && let Some(tempo) = eligibility.tempo_hypothesis
        && let Some((outgoing_start, incoming_start, duration)) = physical_window(
            outgoing,
            incoming,
            config,
            incoming_timeline
                .events
                .iter()
                .find(|event| event.time >= incoming.audible_start)
                .map(|event| event.time)
                .unwrap_or(incoming.audible_start),
        )
    {
        let plan = TransitionPlan {
            kind: TransitionKind::BeatMatched,
            outgoing_start,
            incoming_start,
            incoming_cue_selection: None,
            duration,
            incoming_tempo_ratio: tempo.ratio,
            harmonic_compatibility: harmonic_compatibility(outgoing, incoming),
            incoming_gain: 1.0,
            tempo_envelope: Some(TempoEnvelope::new(
                tempo.ratio,
                tempo.ratio,
                duration,
                Duration::ZERO,
            )),
            energy_selection: None,
        };
        let local_pair_reliability = scale_duration(duration, f64::from(tempo.ratio))
            .map(|mapped_duration| {
                let outgoing_reliability = reliability_for_timeline_window(
                    outgoing_timeline,
                    outgoing_start,
                    outgoing_start.saturating_add(duration),
                )
                .reliability;
                let incoming_reliability = reliability_for_timeline_window(
                    incoming_timeline,
                    incoming_start,
                    incoming_start.saturating_add(mapped_duration),
                )
                .reliability;
                super::reliability::pair_reliability(outgoing_reliability, incoming_reliability)
            })
            .unwrap_or(pair_reliability);
        let cost = TransitionCostBreakdown::for_plan_with_base_gains(
            outgoing,
            incoming,
            &plan,
            (tempo.ratio - 1.0).abs(),
            config.max_tempo_adjustment,
            eligibility.phase_error,
            local_pair_reliability,
            config.min_beat_confidence,
            outgoing_base_gain,
            incoming_base_gain,
        );
        let quality = evaluate_transition_quality_with_base_gains(
            outgoing,
            incoming,
            &plan,
            outgoing_base_gain,
            incoming_base_gain,
        );
        let hard_rejection =
            quality_rejection_with_eligibility(&quality, plan.kind, Some(&eligibility));
        if let Some(reason) = hard_rejection {
            diagnostics.add_reason(reason.reason());
        } else {
            diagnostics.add_candidate(TransitionKind::BeatMatched);
            candidates.push(TransitionCandidate {
                plan,
                cost,
                beat_eligibility: Some(eligibility.clone()),
                hard_rejection: None,
                cue_diagnostics: None,
            });
        }
    }

    let crossfade = crossfade_plan(outgoing, incoming, config);
    if crossfade.kind == TransitionKind::Crossfade {
        let quality = evaluate_transition_quality_with_base_gains(
            outgoing,
            incoming,
            &crossfade,
            outgoing_base_gain,
            incoming_base_gain,
        );
        let hard_rejection = quality_rejection(&quality, crossfade.kind);
        if let Some(reason) = hard_rejection {
            diagnostics.add_reason(reason.reason());
        } else {
            diagnostics.add_candidate(TransitionKind::Crossfade);
            let cost = TransitionCostBreakdown::for_plan_with_base_gains(
                outgoing,
                incoming,
                &crossfade,
                0.0,
                config.max_tempo_adjustment,
                None,
                pair_reliability,
                config.min_beat_confidence,
                outgoing_base_gain,
                incoming_base_gain,
            );
            candidates.push(TransitionCandidate {
                plan: crossfade,
                cost,
                beat_eligibility: Some(eligibility.clone()),
                hard_rejection: None,
                cue_diagnostics: None,
            });
        }
    } else {
        diagnostics.add_reason(AutoMixV2Reason::PhysicalWindowUnavailable);
    }

    let gapless_cost = TransitionCostBreakdown::for_plan_with_base_gains(
        outgoing,
        incoming,
        &gapless,
        0.0,
        config.max_tempo_adjustment,
        None,
        pair_reliability,
        config.min_beat_confidence,
        outgoing_base_gain,
        incoming_base_gain,
    );
    candidates.push(TransitionCandidate {
        plan: gapless,
        cost: gapless_cost,
        beat_eligibility: Some(eligibility.clone()),
        hard_rejection: None,
        cue_diagnostics: None,
    });
    diagnostics.add_candidate(TransitionKind::Gapless);
    candidates.sort_by(|left, right| {
        left.cost
            .total
            .total_cmp(&right.cost.total)
            .then_with(|| strategy_order(left.plan.kind).cmp(&strategy_order(right.plan.kind)))
    });
    let selected = candidates
        .first()
        .cloned()
        .unwrap_or_else(|| TransitionCandidate {
            plan: gapless_plan(outgoing, incoming),
            cost: gapless_cost,
            beat_eligibility: Some(eligibility.clone()),
            hard_rejection: None,
            cue_diagnostics: None,
        });
    let beat_eligible = candidates.iter().any(|candidate| {
        candidate.plan.kind == TransitionKind::BeatMatched && candidate.hard_rejection.is_none()
    });
    diagnostics.set_selection(selected.plan.kind, selected.cost, beat_eligible);
    if selected.plan.kind == TransitionKind::BeatMatched {
        diagnostics.add_reason(AutoMixV2Reason::BeatMatchedSelected);
    } else if beat_eligible && selected.plan.kind == TransitionKind::Crossfade {
        diagnostics.add_reason(AutoMixV2Reason::BeatMatchAvailableButCrossfadePreferred);
        diagnostics.add_reason(AutoMixV2Reason::CrossfadeSelected);
    } else if beat_eligible && selected.plan.kind == TransitionKind::Gapless {
        diagnostics.add_reason(AutoMixV2Reason::BeatMatchAvailableButGaplessPreferred);
        diagnostics.add_reason(AutoMixV2Reason::GaplessSelected);
    } else if selected.plan.kind == TransitionKind::Crossfade {
        diagnostics.add_reason(AutoMixV2Reason::CrossfadeSelected);
    } else {
        diagnostics.add_reason(AutoMixV2Reason::GaplessSelected);
    }
    let plan = TransitionPlanV2 {
        plan: selected.plan,
        cost: selected.cost,
        diagnostics,
        candidates,
    };
    (plan, eligibility)
}

#[allow(clippy::too_many_arguments)]
fn cue_transition_candidates(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    config: &AutoMixConfig,
    outgoing_timeline: &BeatTimeline,
    incoming_timeline: &BeatTimeline,
    outgoing_hypotheses: &[TempoHypothesis],
    incoming_hypotheses: &[TempoHypothesis],
    outgoing_cue_candidates: &[DjCue],
    incoming_cue_candidates: &[DjCue],
    outgoing_phrase_boundaries: &[PhraseBoundary],
    incoming_phrase_boundaries: &[PhraseBoundary],
    blend_config: &BeatmatchBlendConfig,
    outgoing_base_gain: f32,
    incoming_base_gain: f32,
) -> (Vec<TransitionCandidate>, (usize, usize, usize, usize)) {
    if !config.enabled
        || !config.max_tempo_adjustment.is_finite()
        || config.max_tempo_adjustment < 0.0
    {
        return (Vec::new(), (0, 0, 0, 0));
    }
    let outgoing_events = usable_events(outgoing, outgoing_timeline);
    let incoming_events = usable_events(incoming, incoming_timeline);
    if outgoing_events.len() < MIN_V2_BEAT_PAIRS
        || incoming_events.len() < MIN_V2_BEAT_PAIRS
        || !outgoing_timeline.has_strict_times()
        || !incoming_timeline.has_strict_times()
    {
        return (Vec::new(), (0, 0, 0, 0));
    }

    let outgoing_cues = usable_role_cues(
        outgoing_cue_candidates,
        CueRole::MixOut,
        outgoing,
        outgoing_timeline,
        outgoing_phrase_boundaries,
    );
    let incoming_cues = usable_role_cues(
        incoming_cue_candidates,
        CueRole::MixIn,
        incoming,
        incoming_timeline,
        incoming_phrase_boundaries,
    );
    let cue_pair_count = outgoing_cues.len() * incoming_cues.len();
    if outgoing_cues.is_empty() || incoming_cues.is_empty() {
        return (Vec::new(), (outgoing_cues.len(), incoming_cues.len(), 0, 0));
    }

    let tempo_pairs = cross_product_tempo_hypotheses(outgoing_hypotheses, incoming_hypotheses)
        .into_iter()
        .filter(|pair| pair.within_adjustment(config.max_tempo_adjustment))
        .collect::<Vec<_>>();
    if tempo_pairs.is_empty() {
        return (
            Vec::new(),
            (outgoing_cues.len(), incoming_cues.len(), cue_pair_count, 0),
        );
    }

    // Rank all bounded cue/tempo combinations before doing quality evaluation.
    // This keeps actual plan construction to a fixed maximum even when every
    // cue and every tempo interpretation is available.
    let blend_lengths = blend_beat_lengths(blend_config);
    let mut combinations = Vec::with_capacity(
        cue_pair_count
            .saturating_mul(tempo_pairs.len())
            .saturating_mul(blend_lengths.len()),
    );
    for outgoing_cue in &outgoing_cues {
        let Some(outgoing_time) = outgoing_timeline
            .events
            .get(outgoing_cue.beat_index)
            .map(|event| event.time)
        else {
            continue;
        };
        for incoming_cue in &incoming_cues {
            let Some(incoming_time) = incoming_timeline
                .events
                .get(incoming_cue.beat_index)
                .map(|event| event.time)
            else {
                continue;
            };
            let cue_cost = super::scoring::cue_suitability_cost(outgoing_cue, incoming_cue);
            for tempo in &tempo_pairs {
                for &beats in &blend_lengths {
                    let duration_cost = blend_duration_cost(
                        beats,
                        blend_config.preferred_beats,
                        blend_config.min_beats,
                        blend_config.max_beats,
                    );
                    combinations.push((
                        cue_cost + tempo.cost + duration_cost,
                        outgoing_time,
                        incoming_time,
                        *outgoing_cue,
                        *incoming_cue,
                        *tempo,
                        beats,
                    ));
                }
            }
        }
    }
    combinations.sort_by(|left, right| {
        left.0
            .total_cmp(&right.0)
            .then_with(|| left.1.cmp(&right.1))
            .then_with(|| left.2.cmp(&right.2))
            .then_with(|| left.5.ratio.total_cmp(&right.5.ratio))
            .then_with(|| left.6.cmp(&right.6))
    });
    combinations.truncate(MAX_CUE_TRANSITION_CANDIDATES);
    let checked_combinations = combinations.len();
    let incoming_default_start = incoming_events[0].time;
    let mut candidates = Vec::with_capacity(checked_combinations);

    for (_, outgoing_start, incoming_start, outgoing_cue, incoming_cue, tempo, beats) in
        combinations
    {
        // A DJ cue is an anchor, not an instruction to fade all the way to
        // the source's audible end. Choose a bounded phrase-sized family and
        // validate both decks against their physical source bounds.
        let Some(beat_duration) = local_beat_duration(
            outgoing_timeline,
            outgoing_cue.beat_index,
            Some(tempo.outgoing.bpm),
        ) else {
            continue;
        };
        let Some(duration) = beat_duration.checked_mul(beats as u32) else {
            continue;
        };
        if duration < MIN_CUE_OVERLAP
            || outgoing_start
                .checked_add(duration)
                .is_none_or(|end| end > outgoing.audible_end)
        {
            continue;
        }
        let Some(mapped_duration) = scale_duration(duration, f64::from(tempo.ratio)) else {
            continue;
        };
        let Some(mapped_incoming_end) = incoming_start.checked_add(mapped_duration) else {
            continue;
        };
        if incoming_start < incoming.audible_start || mapped_incoming_end > incoming.audible_end {
            continue;
        }
        let (beat_pairs, phase_error) = phase_pairs(
            &outgoing_events,
            &incoming_events,
            outgoing_start,
            incoming_start,
            duration,
            tempo.ratio,
        );
        if beat_pairs < MIN_V2_BEAT_PAIRS {
            continue;
        }
        let local_pair_reliability = pair_reliability(
            reliability_for_timeline_window(
                outgoing_timeline,
                outgoing_start,
                outgoing_start.saturating_add(duration),
            )
            .reliability,
            reliability_for_timeline_window(incoming_timeline, incoming_start, mapped_incoming_end)
                .reliability,
        );
        let plan = TransitionPlan {
            kind: TransitionKind::BeatMatched,
            outgoing_start,
            incoming_start,
            incoming_cue_selection: Some(super::AutoMixIncomingCueSelection {
                default_start: incoming_default_start,
                selected_start: incoming_start,
                candidates_checked: incoming_cues.len(),
            }),
            duration,
            incoming_tempo_ratio: tempo.ratio,
            harmonic_compatibility: harmonic_compatibility(outgoing, incoming),
            incoming_gain: 1.0,
            tempo_envelope: Some(TempoEnvelope::new(
                tempo.ratio,
                tempo.ratio,
                duration,
                Duration::ZERO,
            )),
            energy_selection: None,
        };
        let cue_eligibility = BeatMatchEligibility::eligible(
            outgoing_events.len(),
            incoming_events.len(),
            beat_pairs,
            phase_error,
            tempo,
        );
        let cost = TransitionCostBreakdown::for_plan_with_base_gains(
            outgoing,
            incoming,
            &plan,
            (tempo.ratio - 1.0).abs(),
            config.max_tempo_adjustment,
            phase_error,
            local_pair_reliability,
            config.min_beat_confidence,
            outgoing_base_gain,
            incoming_base_gain,
        )
        .with_cue_suitability(&outgoing_cue, &incoming_cue)
        .with_blend_duration(
            beats,
            blend_config.preferred_beats,
            blend_config.min_beats,
            blend_config.max_beats,
        )
        .with_structure_alignment(structure_alignment_cost(
            outgoing_timeline,
            incoming_timeline,
            outgoing_cue,
            incoming_cue,
            outgoing_phrase_boundaries,
            incoming_phrase_boundaries,
            outgoing_start,
            incoming_start,
            duration,
        ));
        let quality = evaluate_transition_quality_with_base_gains(
            outgoing,
            incoming,
            &plan,
            outgoing_base_gain,
            incoming_base_gain,
        );
        if quality_rejection_with_eligibility(&quality, plan.kind, Some(&cue_eligibility)).is_some()
        {
            continue;
        }
        let cue_suitability_cost = cost.cue_suitability_cost;
        candidates.push(TransitionCandidate {
            plan,
            cost,
            beat_eligibility: Some(cue_eligibility),
            hard_rejection: None,
            cue_diagnostics: Some(CueCandidateDiagnostics {
                outgoing_cue_index: outgoing_cue.beat_index,
                incoming_cue_index: incoming_cue.beat_index,
                cue_score: super::scoring::cue_suitability_score(&outgoing_cue, &incoming_cue),
                cue_suitability_cost,
                tempo_pair: tempo,
                rhythm_reliability: local_pair_reliability,
            }),
        });
    }
    (
        candidates,
        (
            outgoing_cues.len(),
            incoming_cues.len(),
            cue_pair_count,
            checked_combinations,
        ),
    )
}

fn blend_beat_lengths(config: &BeatmatchBlendConfig) -> Vec<usize> {
    let min = config.min_beats.max(1);
    let max = config.max_beats.max(min);
    let mut lengths = BLEND_BEAT_FAMILY
        .into_iter()
        .filter(|beats| *beats >= min && *beats <= max)
        .collect::<Vec<_>>();
    if lengths.is_empty() {
        lengths.push(config.preferred_beats.clamp(min, max));
    }
    lengths.sort_unstable();
    lengths.dedup();
    lengths
}

fn blend_duration_cost(beats: usize, preferred: usize, min: usize, max: usize) -> f32 {
    let preferred = preferred.max(1) as f32;
    let span = max.saturating_sub(min).max(1) as f32;
    (0.12 * ((beats as f32 - preferred).abs() / span.max(preferred)).min(1.0)).max(0.0)
}

fn local_beat_duration(
    timeline: &BeatTimeline,
    beat_index: usize,
    tempo_fallback_bpm: Option<f32>,
) -> Option<Duration> {
    timeline.events.get(beat_index)?;
    let local_start = beat_index.saturating_sub(8);
    let local_end = beat_index.saturating_add(9).min(timeline.events.len());
    let local_intervals = interval_durations(&timeline.events[local_start..local_end]);
    robust_duration_median(&local_intervals)
        .or_else(|| robust_duration_median(&interval_durations(&timeline.events)))
        .or_else(|| tempo_fallback_bpm.and_then(super::beat_interval_from_bpm))
}

fn interval_durations(events: &[TimelineEvent]) -> Vec<Duration> {
    events
        .windows(2)
        .filter_map(|pair| pair[1].time.checked_sub(pair[0].time))
        .filter(|interval| !interval.is_zero() && interval.as_secs_f64().is_finite())
        .collect()
}

fn robust_duration_median(intervals: &[Duration]) -> Option<Duration> {
    if intervals.is_empty() {
        return None;
    }
    let mut sorted = intervals.to_vec();
    sorted.sort_unstable();
    let middle = sorted.len() / 2;
    Some(if sorted.len().is_multiple_of(2) {
        sorted[middle - 1]
            .checked_add(sorted[middle])
            .map_or(sorted[middle], |sum| sum / 2)
    } else {
        sorted[middle]
    })
}

fn usable_role_cues(
    candidates: &[DjCue],
    role: CueRole,
    analysis: &TrackAnalysis,
    timeline: &BeatTimeline,
    _phrase_boundaries: &[PhraseBoundary],
) -> Vec<DjCue> {
    let beat_count = timeline.events.len();
    let filtered = candidates
        .iter()
        .copied()
        .filter(|cue| cue.has_role(role) && cue.validate_for_beat_count(beat_count).is_ok())
        .filter(|cue| {
            timeline.events.get(cue.beat_index).is_some_and(|event| {
                event.time >= analysis.audible_start && event.time < analysis.audible_end
            })
        });
    let bounded = bound_cue_candidates(filtered, beat_count);
    match role {
        CueRole::MixIn => top_mix_in_cues(&bounded),
        CueRole::MixOut => top_mix_out_cues(&bounded),
    }
}

/// Compute a small, finite preference for transitions that use structure
/// evidence well. This is deliberately independent from cue existence: a
/// candidate may cross a detected boundary and remains physically eligible.
#[allow(clippy::too_many_arguments)]
fn structure_alignment_cost(
    outgoing_timeline: &BeatTimeline,
    incoming_timeline: &BeatTimeline,
    outgoing_cue: DjCue,
    incoming_cue: DjCue,
    outgoing_boundaries: &[PhraseBoundary],
    incoming_boundaries: &[PhraseBoundary],
    outgoing_start: Duration,
    incoming_start: Duration,
    duration: Duration,
) -> f32 {
    let outgoing_cue_time = outgoing_timeline.events[outgoing_cue.beat_index].time;
    let incoming_cue_time = incoming_timeline.events[incoming_cue.beat_index].time;
    let outgoing_end = outgoing_start.saturating_add(duration);
    let incoming_end = incoming_start.saturating_add(duration);
    let outgoing_alignment =
        structure_position_alignment(outgoing_timeline, outgoing_boundaries, outgoing_cue_time);
    let incoming_alignment =
        structure_position_alignment(incoming_timeline, incoming_boundaries, incoming_cue_time);
    let outgoing_handoff =
        structure_position_alignment(outgoing_timeline, outgoing_boundaries, outgoing_end);
    let incoming_handoff =
        structure_position_alignment(incoming_timeline, incoming_boundaries, incoming_start);
    let cue_cost = [outgoing_alignment, incoming_alignment]
        .into_iter()
        .flatten()
        .map(|alignment| (1.0 - alignment) * STRUCTURE_CUE_PROXIMITY_WEIGHT)
        .sum::<f32>();
    let handoff_cost = [outgoing_handoff, incoming_handoff]
        .into_iter()
        .flatten()
        .map(|alignment| (1.0 - alignment) * STRUCTURE_HANDOFF_ALIGNMENT_WEIGHT)
        .sum::<f32>();
    let crossing_cost = [
        structure_boundary_crossing_cost(
            outgoing_timeline,
            outgoing_boundaries,
            outgoing_start,
            outgoing_end,
        ),
        structure_boundary_crossing_cost(
            incoming_timeline,
            incoming_boundaries,
            incoming_start,
            incoming_end,
        ),
    ]
    .into_iter()
    .sum::<f32>();
    finite_structure_cost(cue_cost + handoff_cost + crossing_cost)
}

fn structure_position_alignment(
    timeline: &BeatTimeline,
    boundaries: &[PhraseBoundary],
    position: Duration,
) -> Option<f32> {
    let local_interval = interval_durations(&timeline.events)
        .into_iter()
        .min()
        .unwrap_or(Duration::from_secs(1));
    let radius = local_interval.saturating_mul(4);
    boundaries
        .iter()
        .filter_map(|boundary| {
            let time = timeline.events.get(boundary.beat_index)?.time;
            let proximity = if radius.is_zero() {
                0.0
            } else {
                (1.0 - time.abs_diff(position).as_secs_f32() / radius.as_secs_f32()).clamp(0.0, 1.0)
            };
            let source_weight = if boundary.source.is_periodic_prior() {
                STRUCTURE_PERIODIC_PRIOR_WEIGHT
            } else {
                1.0
            };
            Some(proximity * boundary.strength.get() * source_weight)
        })
        .max_by(f32::total_cmp)
}

fn structure_boundary_crossing_cost(
    timeline: &BeatTimeline,
    boundaries: &[PhraseBoundary],
    start: Duration,
    end: Duration,
) -> f32 {
    boundaries
        .iter()
        .filter_map(|boundary| {
            let time = timeline.events.get(boundary.beat_index)?.time;
            (time > start && time < end).then_some(
                boundary.strength.get()
                    * if boundary.source.is_periodic_prior() {
                        STRUCTURE_PERIODIC_PRIOR_WEIGHT
                    } else {
                        1.0
                    },
            )
        })
        .map(|strength| strength * STRUCTURE_BOUNDARY_CROSSING_WEIGHT)
        .sum()
}

fn finite_structure_cost(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, MAX_STRUCTURE_ALIGNMENT_COST)
    } else {
        0.0
    }
}

fn usable_events(analysis: &TrackAnalysis, timeline: &BeatTimeline) -> Vec<TimelineEvent> {
    timeline
        .events
        .iter()
        .copied()
        .filter(|event| event.time >= analysis.audible_start && event.time <= analysis.audible_end)
        .collect()
}

fn physical_window(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    config: &AutoMixConfig,
    incoming_start: Duration,
) -> Option<(Duration, Duration, Duration)> {
    if !config.enabled
        || outgoing.audible_end <= outgoing.audible_start
        || incoming.audible_end <= incoming_start
        || config.crossfade.is_zero()
    {
        return None;
    }
    let available_outgoing = outgoing.audible_end.saturating_sub(outgoing.audible_start);
    let available_incoming = incoming.audible_end.saturating_sub(incoming_start);
    let timing = plan_transition_timing(available_outgoing, available_incoming, config.crossfade)?;
    let duration = timing.fade_duration;
    let outgoing_start = outgoing.audible_end.saturating_sub(duration);
    (duration > Duration::ZERO
        && outgoing_start >= outgoing.audible_start
        && incoming_start >= incoming.audible_start
        && incoming_start.saturating_add(duration) <= incoming.audible_end)
        .then_some((outgoing_start, incoming_start, duration))
}

fn phase_pairs(
    outgoing: &[TimelineEvent],
    incoming: &[TimelineEvent],
    outgoing_start: Duration,
    incoming_start: Duration,
    duration: Duration,
    ratio: f32,
) -> (usize, Option<Duration>) {
    if !ratio.is_finite() || ratio <= 0.0 {
        return (0, None);
    }
    let outgoing_end = outgoing_start.saturating_add(duration);
    let Some(mapped_duration) = scale_duration(duration, f64::from(ratio)) else {
        return (0, None);
    };
    let incoming_end = incoming_start.saturating_add(mapped_duration);
    let mut pairs = 0;
    let mut max_error = Duration::ZERO;
    let mut previous_incoming = None;
    for event in outgoing
        .iter()
        .filter(|event| event.time >= outgoing_start && event.time <= outgoing_end)
    {
        let elapsed = event.time.saturating_sub(outgoing_start);
        let Some(mapped_elapsed) = scale_duration(elapsed, f64::from(ratio)) else {
            continue;
        };
        let expected = incoming_start.saturating_add(mapped_elapsed);
        let Some(nearest) = incoming
            .iter()
            .filter(|candidate| {
                candidate.time >= incoming_start
                    && candidate.time <= incoming_end
                    && previous_incoming.is_none_or(|previous| candidate.time > previous)
            })
            .min_by_key(|candidate| candidate.time.abs_diff(expected))
        else {
            continue;
        };
        // Phase precision is measured against the mapped event clock.  This
        // retains cumulative drift as a cost signal even when the separate
        // quality guard later decides that the drift is a hard failure.
        let error = nearest.time.abs_diff(expected);
        pairs += 1;
        max_error = max_error.max(error);
        previous_incoming = Some(nearest.time);
    }
    (pairs, (pairs > 0).then_some(max_error))
}

fn scale_duration(duration: Duration, factor: f64) -> Option<Duration> {
    if !factor.is_finite() || factor < 0.0 {
        return None;
    }
    let seconds = duration.as_secs_f64() * factor;
    (seconds.is_finite() && seconds <= Duration::MAX.as_secs_f64())
        .then(|| Duration::from_secs_f64(seconds))
}

fn strategy_order(kind: TransitionKind) -> u8 {
    match kind {
        TransitionKind::BeatMatched => 0,
        TransitionKind::Crossfade => 1,
        TransitionKind::Gapless => 2,
    }
}

fn crossfade_plan(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    config: &AutoMixConfig,
) -> TransitionPlan {
    if !config.enabled {
        return gapless_plan(outgoing, incoming);
    }
    let available_outgoing = outgoing.audible_end.saturating_sub(outgoing.audible_start);
    let available_incoming = incoming.audible_end.saturating_sub(incoming.audible_start);
    let Some(timing) =
        plan_transition_timing(available_outgoing, available_incoming, config.crossfade)
    else {
        return gapless_plan(outgoing, incoming);
    };
    let duration = timing.fade_duration;
    TransitionPlan {
        kind: TransitionKind::Crossfade,
        outgoing_start: outgoing.audible_end.saturating_sub(duration),
        incoming_start: incoming.audible_start,
        incoming_cue_selection: None,
        duration,
        incoming_tempo_ratio: 1.0,
        harmonic_compatibility: harmonic_compatibility(outgoing, incoming),
        incoming_gain: 1.0,
        tempo_envelope: None,
        energy_selection: None,
    }
}

fn gapless_plan(outgoing: &TrackAnalysis, incoming: &TrackAnalysis) -> TransitionPlan {
    TransitionPlan {
        kind: TransitionKind::Gapless,
        outgoing_start: outgoing.audible_end,
        incoming_start: incoming.audible_start,
        incoming_cue_selection: None,
        duration: Duration::ZERO,
        incoming_tempo_ratio: 1.0,
        harmonic_compatibility: harmonic_compatibility(outgoing, incoming),
        incoming_gain: 1.0,
        tempo_envelope: None,
        energy_selection: None,
    }
}

fn quality_has_v2_hard_issue(quality: &AutoMixQualityReport, kind: TransitionKind) -> bool {
    quality.issues.iter().any(|issue| match issue {
        AutoMixQualityIssue::MixOverlapTooShort { .. }
        | AutoMixQualityIssue::IncomingOverlapExceedsAudibleEnd { .. }
        | AutoMixQualityIssue::BeatPhaseDriftTooLarge { .. }
        | AutoMixQualityIssue::BeatHandoffPhaseDriftTooLarge { .. }
        | AutoMixQualityIssue::DualVocalOverlapTooHigh { .. }
        | AutoMixQualityIssue::MixEnergyDipTooDeep { .. } => true,
        // V1's confidence/support/downbeat/phrase verification gates are
        // preferences in V2.  BeatPhaseUnverified is hard only when the V2
        // candidate itself has no observed pair; that is checked before this
        // function is called.
        AutoMixQualityIssue::BeatPhaseUnverified
        | AutoMixQualityIssue::OutgoingOverlapMissesAudibleEnd { .. }
        | AutoMixQualityIssue::DownbeatPhaseUnverified
        | AutoMixQualityIssue::DownbeatPhaseDriftTooLarge { .. }
        | AutoMixQualityIssue::DownbeatHandoffPhaseDriftTooLarge { .. }
        | AutoMixQualityIssue::PhrasePhaseDriftTooLarge { .. }
        | AutoMixQualityIssue::PhraseHandoffPhaseDriftTooLarge { .. }
        | AutoMixQualityIssue::LowHandoffDip { .. }
        | AutoMixQualityIssue::LowHandoffBuildUp { .. } => false,
    }) || !plan_is_finite_and_bounded(quality, kind)
}

fn quality_rejection(
    quality: &AutoMixQualityReport,
    kind: TransitionKind,
) -> Option<BeatMatchRejection> {
    if !quality_has_v2_hard_issue(quality, kind) {
        return None;
    }
    if kind == TransitionKind::BeatMatched
        && quality.issues.iter().any(|issue| {
            matches!(
                issue,
                AutoMixQualityIssue::BeatPhaseDriftTooLarge { .. }
                    | AutoMixQualityIssue::BeatHandoffPhaseDriftTooLarge { .. }
            )
        })
    {
        Some(BeatMatchRejection::PhaseDriftTooLarge)
    } else {
        Some(BeatMatchRejection::QualityGuardRejected)
    }
}

fn quality_rejection_with_eligibility(
    quality: &AutoMixQualityReport,
    kind: TransitionKind,
    eligibility: Option<&BeatMatchEligibility>,
) -> Option<BeatMatchRejection> {
    if kind == TransitionKind::BeatMatched
        && eligibility.is_some_and(|eligibility| {
            eligibility.beat_pairs >= MIN_V2_BEAT_PAIRS
                && eligibility
                    .phase_error
                    .is_some_and(|error| error > Duration::from_millis(35))
        })
    {
        return Some(BeatMatchRejection::PhaseDriftTooLarge);
    }
    quality_rejection(quality, kind)
}

/// Keep the legacy report's measurements while dropping V1-only blocking
/// labels from the V2-facing quality value.  This is what makes marker
/// support, global confidence, downbeat confidence, and phrase priors soft
/// preferences without mutating the legacy evaluator itself.
fn v2_quality_report(quality: &AutoMixQualityReport, kind: TransitionKind) -> AutoMixQualityReport {
    let mut filtered = quality.clone();
    filtered.issues.retain(|issue| match issue {
        AutoMixQualityIssue::MixOverlapTooShort { .. }
        | AutoMixQualityIssue::IncomingOverlapExceedsAudibleEnd { .. }
        | AutoMixQualityIssue::BeatPhaseDriftTooLarge { .. }
        | AutoMixQualityIssue::BeatHandoffPhaseDriftTooLarge { .. }
        | AutoMixQualityIssue::DualVocalOverlapTooHigh { .. }
        | AutoMixQualityIssue::MixEnergyDipTooDeep { .. } => true,
        AutoMixQualityIssue::BeatPhaseUnverified
        | AutoMixQualityIssue::OutgoingOverlapMissesAudibleEnd { .. }
        | AutoMixQualityIssue::DownbeatPhaseUnverified
        | AutoMixQualityIssue::DownbeatPhaseDriftTooLarge { .. }
        | AutoMixQualityIssue::DownbeatHandoffPhaseDriftTooLarge { .. }
        | AutoMixQualityIssue::PhrasePhaseDriftTooLarge { .. }
        | AutoMixQualityIssue::PhraseHandoffPhaseDriftTooLarge { .. }
        | AutoMixQualityIssue::LowHandoffDip { .. }
        | AutoMixQualityIssue::LowHandoffBuildUp { .. } => false,
    });
    // A non-BeatMatched plan cannot have meaningful phase verification.  Any
    // phase issue accidentally emitted by the legacy evaluator is therefore a
    // soft metadata note for V2.
    if kind != TransitionKind::BeatMatched {
        filtered.issues.retain(|issue| {
            !matches!(
                issue,
                AutoMixQualityIssue::BeatPhaseDriftTooLarge { .. }
                    | AutoMixQualityIssue::BeatHandoffPhaseDriftTooLarge { .. }
            )
        });
    }
    filtered
}

fn plan_is_finite_and_bounded(quality: &AutoMixQualityReport, _kind: TransitionKind) -> bool {
    let bounded01 = |value: Option<f32>| {
        value.is_none_or(|value| value.is_finite() && (0.0..=1.0).contains(&value))
    };
    let nonnegative =
        |value: Option<f32>| value.is_none_or(|value| value.is_finite() && value >= 0.0);
    quality.overlap.as_secs_f64().is_finite()
        && bounded01(quality.beat_phase_coverage)
        && bounded01(quality.structure_overlap_ratio)
        && bounded01(quality.harmonic_compatibility)
        && nonnegative(quality.low_handoff_min)
        && nonnegative(quality.low_handoff_max)
        && bounded01(quality.max_dual_vocal_risk)
        && nonnegative(quality.min_mix_energy_ratio)
        && nonnegative(quality.max_mix_energy_ratio)
        && nonnegative(quality.max_mix_energy_step)
        && nonnegative(quality.handoff_mix_energy_ratio)
        && bounded01(quality.handoff_incoming_mix_share)
        && nonnegative(quality.max_tempo_speed_step)
        && quality.issues.iter().all(quality_issue_is_finite)
}

fn quality_issue_is_finite(issue: &AutoMixQualityIssue) -> bool {
    match issue {
        AutoMixQualityIssue::LowHandoffDip { min_gain } => {
            min_gain.is_finite() && (0.0..=2.0).contains(min_gain)
        }
        AutoMixQualityIssue::LowHandoffBuildUp { max_gain } => {
            max_gain.is_finite() && (0.0..=2.0).contains(max_gain)
        }
        AutoMixQualityIssue::DualVocalOverlapTooHigh { max_risk } => {
            max_risk.is_finite() && (0.0..=1.0).contains(max_risk)
        }
        AutoMixQualityIssue::MixEnergyDipTooDeep { min_ratio } => {
            min_ratio.is_finite() && *min_ratio >= 0.0
        }
        AutoMixQualityIssue::MixOverlapTooShort { .. }
        | AutoMixQualityIssue::OutgoingOverlapMissesAudibleEnd { .. }
        | AutoMixQualityIssue::IncomingOverlapExceedsAudibleEnd { .. }
        | AutoMixQualityIssue::BeatPhaseUnverified
        | AutoMixQualityIssue::DownbeatPhaseUnverified
        | AutoMixQualityIssue::BeatPhaseDriftTooLarge { .. }
        | AutoMixQualityIssue::BeatHandoffPhaseDriftTooLarge { .. }
        | AutoMixQualityIssue::DownbeatPhaseDriftTooLarge { .. }
        | AutoMixQualityIssue::DownbeatHandoffPhaseDriftTooLarge { .. }
        | AutoMixQualityIssue::PhrasePhaseDriftTooLarge { .. }
        | AutoMixQualityIssue::PhraseHandoffPhaseDriftTooLarge { .. } => true,
    }
}

// Keep these imports/functions visible to downstream code that used the
// module-level adapters while V2 was being integrated.
pub fn v2_pair_reliability(outgoing: &TrackAnalysis, incoming: &TrackAnalysis) -> f32 {
    compute_pair_reliability(outgoing, incoming)
}

pub fn v2_rhythm_reliability(analysis: &TrackAnalysis) -> f32 {
    reliability_for_analysis(analysis).reliability
}

pub fn v2_rhythm_uncertainty_cost(analysis: &TrackAnalysis, target: f32) -> f32 {
    rhythm_uncertainty_cost(v2_rhythm_reliability(analysis), target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::{
        BeatEvent, Confidence, MeterHypothesis, ModelScore, PhraseBoundary, Section, SectionLabel,
        Support, TempoHypothesis, TempoRelation, TrackAnalysisV2, UnitInterval,
    };
    use crate::automix::evaluate_transition_quality;

    fn config() -> AutoMixConfig {
        AutoMixConfig {
            enabled: true,
            crossfade: Duration::from_secs(8),
            max_tempo_adjustment: 0.05,
            min_beat_confidence: 0.70,
        }
    }

    fn analysis_with_markers(times: &[f32], confidence: f32, bpm: f32) -> TrackAnalysis {
        let mut analysis = TrackAnalysis::unanalyzed(Duration::from_secs(180));
        analysis.bpm = Some(bpm);
        analysis.beat_confidence = 0.95;
        analysis.beat_markers = times
            .iter()
            .map(|time| Duration::from_secs_f32(*time))
            .collect();
        analysis.beat_marker_confidences = vec![confidence; times.len()];
        analysis.first_beat = analysis.beat_markers.first().copied();
        analysis
    }

    fn paired_edge_analyses(confidence: f32) -> (TrackAnalysis, TrackAnalysis) {
        let outgoing = analysis_with_markers(
            &[172.0, 173.0, 174.0, 175.0, 176.0, 177.0, 178.0, 179.0],
            confidence,
            60.0,
        );
        let incoming =
            analysis_with_markers(&[0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0], confidence, 60.0);
        (outgoing, incoming)
    }

    fn v2_analysis_with_events(times: &[f32]) -> TrackAnalysisV2 {
        let mut analysis = TrackAnalysisV2::unanalyzed(Duration::from_secs(180));
        analysis.rhythm.beats = times
            .iter()
            .map(|time| {
                BeatEvent::new(
                    Duration::from_secs_f32(*time),
                    Some(ModelScore::new(0.95).unwrap()),
                    None,
                    Confidence::new(0.95).unwrap(),
                    Some(Support::new(0.95).unwrap()),
                    Some(Support::new(0.05).unwrap()),
                )
            })
            .collect();
        analysis.rhythm.tempo_hypotheses.push(TempoHypothesis {
            bpm: 60.0,
            relative_weight: UnitInterval::ONE,
            relation: TempoRelation::Primary,
        });
        analysis
    }

    #[test]
    fn local_beat_duration_ignores_a_single_outlier_interval() {
        let timeline = BeatTimeline::from_times(
            [0_u64, 1, 2, 3, 4, 5, 6, 7, 8, 1_008]
                .into_iter()
                .map(Duration::from_secs),
        );

        assert_eq!(
            local_beat_duration(&timeline, 8, Some(60.0)),
            Some(Duration::from_secs(1))
        );
        assert_eq!(
            local_beat_duration(&BeatTimeline::from_times([Duration::ZERO]), 0, Some(60.0)),
            Some(Duration::from_secs(1))
        );
    }

    #[test]
    fn structure_alignment_is_a_finite_ranking_cost_not_a_rejection() {
        let outgoing_timeline = BeatTimeline::from_times((0_u64..=12).map(Duration::from_secs));
        let incoming_timeline = BeatTimeline::from_times((0_u64..=12).map(Duration::from_secs));
        let outgoing_cue = DjCue::new(4, UnitInterval::ONE);
        let incoming_cue = DjCue::new(0, UnitInterval::ONE);
        let aligned_boundaries = [
            PhraseBoundary::detected(4, UnitInterval::ONE),
            PhraseBoundary::detected(8, UnitInterval::ONE),
        ];
        let aligned_incoming_boundaries = [
            PhraseBoundary::detected(0, UnitInterval::ONE),
            PhraseBoundary::detected(4, UnitInterval::ONE),
        ];
        let misaligned_boundaries = [
            PhraseBoundary::detected(2, UnitInterval::ONE),
            PhraseBoundary::detected(10, UnitInterval::ONE),
        ];

        let aligned = structure_alignment_cost(
            &outgoing_timeline,
            &incoming_timeline,
            outgoing_cue,
            incoming_cue,
            &aligned_boundaries,
            &aligned_incoming_boundaries,
            Duration::from_secs(4),
            Duration::ZERO,
            Duration::from_secs(4),
        );
        let misaligned = structure_alignment_cost(
            &outgoing_timeline,
            &incoming_timeline,
            outgoing_cue,
            incoming_cue,
            &misaligned_boundaries,
            &misaligned_boundaries,
            Duration::from_secs(4),
            Duration::ZERO,
            Duration::from_secs(4),
        );

        assert!(aligned.is_finite() && misaligned.is_finite());
        assert!(
            aligned < misaligned,
            "aligned={aligned}, misaligned={misaligned}"
        );
    }

    #[test]
    fn v2_quality_and_cost_use_runtime_base_gains_once() {
        let (mut outgoing, mut incoming) = paired_edge_analyses(0.95);
        outgoing.energy_profile = vec![200; 180];
        outgoing.energy_profile_rate = 1;
        incoming.energy_profile = vec![150; 180];
        incoming.energy_profile_rate = 1;
        let blend = BeatmatchBlendConfig::default();
        let guarded = plan_guarded_transition_v2_with_blend_config_and_base_gains(
            &outgoing,
            &incoming,
            &config(),
            &blend,
            0.5,
            0.75,
        );
        let expected_quality = v2_quality_report(
            &evaluate_transition_quality_with_base_gains(
                &outgoing,
                &incoming,
                &guarded.plan,
                0.5,
                0.75,
            ),
            guarded.plan.kind,
        );
        assert_eq!(guarded.quality, expected_quality);

        let planned = plan_transition_v2_with_blend_config_and_base_gains(
            &outgoing,
            &incoming,
            &config(),
            &blend,
            0.5,
            0.75,
        );
        let selected = planned
            .candidates
            .iter()
            .find(|candidate| candidate.plan == planned.plan)
            .expect("selected V2 candidate");
        let expected_legacy_quality = crate::automix::transition_score_breakdown(
            &evaluate_transition_quality_with_base_gains(
                &outgoing,
                &incoming,
                &planned.plan,
                0.5,
                0.75,
            ),
        )
        .map_or(0.0, |breakdown| breakdown.total.max(0.0));
        assert!((selected.cost.legacy_quality_cost - expected_legacy_quality).abs() < 0.00001);
        assert!(selected.cost.total.is_finite());
        assert!(planned.candidates.iter().all(|candidate| {
            candidate.cost.total.is_finite()
                && candidate.cost.structure_alignment_cost.is_finite()
                && candidate.cost.legacy_quality_cost.is_finite()
        }));
    }

    #[test]
    fn planner_uses_bounded_valid_merged_role_cues_as_transition_positions() {
        let mut outgoing = v2_analysis_with_events(&[
            172.0, 173.0, 174.0, 175.0, 176.0, 177.0, 178.0, 179.0, 180.0, 181.0,
        ]);
        outgoing.duration = Duration::from_secs(200);
        outgoing.audible_end = Duration::from_secs(182);
        let mut incoming =
            v2_analysis_with_events(&[0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0]);
        incoming.duration = Duration::from_secs(100);
        incoming.audible_end = Duration::from_secs(100);

        // Duplicate beats merge field-wise; malformed/out-of-grid cues are
        // discarded before taking the per-role top eight.
        for beat_index in 0..10 {
            let mut cue = DjCue::new(beat_index, UnitInterval::clamped(0.5));
            cue.mix_out = UnitInterval::clamped(if beat_index == 2 { 0.8 } else { 0.4 });
            if beat_index == 2 {
                cue.importance = UnitInterval::clamped(0.9);
            }
            outgoing.cues.push(cue);

            let mut cue = DjCue::new(beat_index, UnitInterval::clamped(0.3));
            cue.mix_in = UnitInterval::clamped(if beat_index == 1 { 0.9 } else { 0.4 });
            if beat_index == 1 {
                cue.importance = UnitInterval::ONE;
            }
            incoming.cues.push(cue);
        }
        let mut duplicate = DjCue::new(2, UnitInterval::clamped(0.1));
        duplicate.mix_out = UnitInterval::ONE;
        outgoing.cues.push(duplicate);
        let mut invalid = DjCue::new(usize::MAX, UnitInterval::ONE);
        invalid.mix_out = UnitInterval::ONE;
        outgoing.cues.push(invalid);

        outgoing
            .structure
            .phrase_boundaries
            .push(PhraseBoundary::detected(0, UnitInterval::ONE));
        incoming
            .structure
            .phrase_boundaries
            .push(PhraseBoundary::detected(9, UnitInterval::ONE));

        let planned = plan_transition_v2_for_analysis(&outgoing, &incoming, &config());
        let repeated = plan_transition_v2_for_analysis(&outgoing, &incoming, &config());
        assert_eq!(planned, repeated);
        assert_eq!(planned.diagnostics.outgoing_mix_out_cues, 8);
        assert_eq!(planned.diagnostics.incoming_mix_in_cues, 8);
        assert_eq!(planned.diagnostics.cue_pairs_checked, 64);
        assert!(planned.diagnostics.cue_tempo_combinations_checked <= 64);
        assert!(planned.diagnostics.beatmatched_candidates <= 64);
        assert_eq!(
            planned.plan.kind,
            TransitionKind::BeatMatched,
            "{planned:?}"
        );
        assert_eq!(planned.plan.outgoing_start, Duration::from_secs(174));
        assert_eq!(planned.plan.incoming_start, Duration::from_secs(1));
        assert_eq!(planned.plan.duration, Duration::from_secs(8));
        assert_eq!(
            planned
                .plan
                .incoming_cue_selection
                .expect("cue-backed incoming position")
                .selected_start,
            Duration::from_secs(1)
        );
        assert!(
            planned.cost.cue_suitability_cost < TransitionCostBreakdown::MAX_CUE_SUITABILITY_COST
        );
        let selected_cue = planned
            .candidates
            .iter()
            .find(|candidate| {
                candidate.plan.kind == TransitionKind::BeatMatched
                    && candidate.plan.outgoing_start == Duration::from_secs(174)
                    && candidate.plan.incoming_start == Duration::from_secs(1)
            })
            .and_then(|candidate| candidate.cue_diagnostics)
            .expect("cue-backed candidate diagnostics");
        assert_eq!(selected_cue.outgoing_cue_index, 2);
        assert_eq!(selected_cue.incoming_cue_index, 1);
        assert!(selected_cue.cue_score.is_finite());
        assert!(selected_cue.cue_suitability_cost.is_finite());
        assert!(selected_cue.tempo_pair.cost.is_finite());
        assert!(selected_cue.rhythm_reliability.is_finite());
    }

    #[test]
    fn low_cue_scores_remain_soft_and_do_not_reject_physical_candidates() {
        let mut outgoing =
            v2_analysis_with_events(&[172.0, 173.0, 174.0, 175.0, 176.0, 177.0, 178.0, 179.0]);
        outgoing.audible_end = Duration::from_secs(180);
        let mut incoming = v2_analysis_with_events(&[0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0]);
        let mut outgoing_cue = DjCue::new(0, UnitInterval::ZERO);
        outgoing_cue.mix_out = UnitInterval::clamped(0.01);
        let mut incoming_cue = DjCue::new(0, UnitInterval::ZERO);
        incoming_cue.mix_in = UnitInterval::clamped(0.01);
        outgoing.cues.push(outgoing_cue);
        incoming.cues.push(incoming_cue);

        let planned = plan_transition_v2_for_analysis(&outgoing, &incoming, &config());
        let cue_candidate = planned.candidates.iter().find(|candidate| {
            candidate.plan.kind == TransitionKind::BeatMatched
                && candidate.plan.outgoing_start == Duration::from_secs(172)
                && candidate.plan.incoming_start.is_zero()
        });
        assert!(cue_candidate.is_some(), "{planned:?}");
        assert_eq!(cue_candidate.unwrap().hard_rejection, None);
        assert!(cue_candidate.unwrap().cost.cue_suitability_cost > 0.0);
    }

    #[test]
    fn tempo_hypothesis_cross_product_is_bounded_and_repeatable() {
        let hypotheses = (60..80)
            .map(|bpm| {
                TempoHypothesis::with_relation(
                    bpm as f32,
                    UnitInterval::clamped((bpm - 60) as f32 / 20.0),
                    TempoRelation::Alternative,
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        let first = cross_product_tempo_hypotheses(&hypotheses, &hypotheses);
        let second = cross_product_tempo_hypotheses(&hypotheses, &hypotheses);
        assert!(first.len() <= 64);
        assert_eq!(first, second);
    }

    #[test]
    fn tempo_pair_cost_exposes_stretch_confidence_and_relation_components() {
        let hypothesis = |bpm, weight, relation| {
            TempoHypothesis::with_relation(bpm, UnitInterval::clamped(weight), relation).unwrap()
        };
        let primary = TempoHypothesisPair::new(
            hypothesis(120.0, 0.25, TempoRelation::Primary),
            hypothesis(120.0, 0.64, TempoRelation::Primary),
        )
        .unwrap();
        assert_eq!(primary.stretch_cost(), 0.0);
        assert!((primary.weight - 0.4).abs() < f32::EPSILON);
        assert!((primary.weight_cost() - 0.012).abs() < 0.00001);
        assert_eq!(primary.relation_penalty(), 0.0);
        assert!((primary.cost - (primary.stretch_cost() + primary.weight_cost())).abs() < 0.00001);

        let half_time = TempoHypothesisPair::new(
            hypothesis(120.0, 1.0, TempoRelation::HalfTime),
            hypothesis(120.0, 1.0, TempoRelation::Primary),
        )
        .unwrap();
        let alternative = TempoHypothesisPair::new(
            hypothesis(120.0, 1.0, TempoRelation::Alternative),
            hypothesis(120.0, 1.0, TempoRelation::Primary),
        )
        .unwrap();
        assert!((half_time.relation_penalty() - 0.001).abs() < f32::EPSILON);
        assert!((alternative.relation_penalty() - 0.003).abs() < f32::EPSILON);
        assert!(alternative.cost > half_time.cost);
    }

    #[test]
    fn strong_half_time_hypothesis_can_win_pair_selection() {
        let primary = TempoHypothesis::with_relation(
            120.0,
            UnitInterval::clamped(0.20),
            TempoRelation::Primary,
        )
        .unwrap();
        let half_time =
            TempoHypothesis::with_relation(120.0, UnitInterval::ONE, TempoRelation::HalfTime)
                .unwrap();
        let incoming =
            TempoHypothesis::with_relation(120.0, UnitInterval::ONE, TempoRelation::Primary)
                .unwrap();

        let selected = select_tempo_hypothesis_pair(&[primary, half_time], &[incoming], 0.05)
            .expect("compatible hypothesis pair");
        assert_eq!(selected.outgoing.relation, TempoRelation::HalfTime);
        assert!(selected.cost < TempoHypothesisPair::new(primary, incoming).unwrap().cost);
    }

    #[test]
    fn strong_beat_events_with_weak_low_band_remain_beatmatched_eligible() {
        let (outgoing, incoming) = paired_edge_analyses(0.05);
        let eligibility = beat_match_eligibility(&outgoing, &incoming, &config());

        assert!(eligibility.eligible, "eligibility={eligibility:?}");
        assert_eq!(eligibility.rejection, None);
        assert_eq!(eligibility.beat_pairs, 8);
    }

    #[test]
    fn v2_strong_beat_this_events_remain_eligible_when_low_band_support_is_weak() {
        let outgoing =
            v2_analysis_with_events(&[172.0, 173.0, 174.0, 175.0, 176.0, 177.0, 178.0, 179.0]);
        let incoming = v2_analysis_with_events(&[0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0]);

        let eligibility = beat_match_eligibility(&outgoing, &incoming, &config());
        assert!(eligibility.eligible, "eligibility={eligibility:?}");
        assert_eq!(eligibility.rejection, None);
        assert_eq!(eligibility.beat_pairs, 8);

        let planned = plan_transition_v2_for_analysis(&outgoing, &incoming, &config());
        assert!(planned.diagnostics.beatmatched_candidates > 0);
        assert!(planned.candidates.iter().any(|candidate| {
            candidate.plan.kind == TransitionKind::BeatMatched && candidate.hard_rejection.is_none()
        }));
    }

    #[test]
    fn manual_v2_events_derive_a_tempo_hypothesis_without_regenerating_events() {
        let mut outgoing =
            v2_analysis_with_events(&[172.0, 173.0, 174.0, 175.0, 176.0, 177.0, 178.0, 179.0]);
        let mut incoming = v2_analysis_with_events(&[0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0]);
        outgoing.rhythm.tempo_hypotheses.clear();
        incoming.rhythm.tempo_hypotheses.clear();

        let eligibility = beat_match_eligibility(&outgoing, &incoming, &config());
        assert!(eligibility.eligible, "eligibility={eligibility:?}");
        assert_eq!(eligibility.beat_pairs, 8);
        assert_eq!(outgoing.rhythm.beats.len(), 8);
        assert_eq!(incoming.rhythm.beats.len(), 8);

        let planned = plan_transition_v2_for_analysis(&outgoing, &incoming, &config());
        assert!(planned.diagnostics.beatmatched_candidates > 0);
    }

    #[test]
    fn unresolved_meter_and_downbeat_scores_do_not_become_legacy_phase_facts() {
        let times_out = [172.0, 173.0, 174.0, 175.0, 176.0, 177.0, 178.0, 179.0];
        let times_in = [0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0];
        let mut outgoing = v2_analysis_with_events(&times_out);
        let mut incoming = v2_analysis_with_events(&times_in);
        for analysis in [&mut outgoing, &mut incoming] {
            analysis.rhythm.beats[0].downbeat_model_score = Some(ModelScore::new(0.99).unwrap());
            analysis.rhythm.meter_hypotheses = vec![
                MeterHypothesis {
                    beats_per_bar: 4,
                    downbeat_phase: 0,
                    score: UnitInterval::clamped(0.80),
                },
                MeterHypothesis {
                    beats_per_bar: 3,
                    downbeat_phase: 1,
                    score: UnitInterval::clamped(0.70),
                },
            ];
            assert_eq!(analysis.rhythm.resolved_meter(), None);
            let legacy = analysis.as_v2_legacy_view();
            assert_eq!(legacy.first_downbeat, None);
            assert_eq!(legacy.downbeat_confidence, 0.0);
        }

        let planned = plan_transition_v2_for_analysis(&outgoing, &incoming, &config());
        assert!(planned.diagnostics.beatmatched_candidates > 0);
        assert!(planned.candidates.iter().any(|candidate| {
            candidate.plan.kind == TransitionKind::BeatMatched && candidate.hard_rejection.is_none()
        }));
    }

    #[test]
    fn periodic_phrase_prior_mismatch_cannot_hard_reject_a_v2_candidate() {
        let prior = crate::analysis::periodic_phrase_prior(64, 0, 4);
        assert!(!crate::analysis::phrase_mismatch_requires_observed_evidence(16, 17, &prior));

        let (outgoing, incoming) = paired_edge_analyses(0.95);
        let plan = TransitionPlan {
            kind: TransitionKind::BeatMatched,
            outgoing_start: Duration::from_secs(172),
            incoming_start: Duration::ZERO,
            incoming_cue_selection: None,
            duration: Duration::from_secs(8),
            incoming_tempo_ratio: 1.0,
            harmonic_compatibility: None,
            incoming_gain: 1.0,
            tempo_envelope: None,
            energy_selection: None,
        };
        let mut quality = evaluate_transition_quality(&outgoing, &incoming, &plan);
        quality.issues = vec![AutoMixQualityIssue::PhrasePhaseDriftTooLarge {
            max_error: Duration::from_secs(1),
        }];

        assert_eq!(
            quality_rejection(&quality, TransitionKind::BeatMatched),
            None
        );
        assert!(
            v2_quality_report(&quality, TransitionKind::BeatMatched)
                .issues
                .is_empty()
        );
    }

    #[test]
    fn v2_adapter_preserves_quality_components_for_legacy_evaluation() {
        let mut analysis =
            v2_analysis_with_events(&[172.0, 173.0, 174.0, 175.0, 176.0, 177.0, 178.0, 179.0]);
        analysis.vocal.activity = vec![UnitInterval::clamped(0.25), UnitInterval::ONE];
        analysis.vocal.confidences = vec![Confidence::clamped(0.5), Confidence::ONE];
        analysis.vocal.rate_hz = 2;
        analysis.energy.profile = vec![UnitInterval::clamped(0.2), UnitInterval::clamped(0.8)];
        analysis.energy.confidences = vec![Confidence::ONE, Confidence::ONE];
        analysis.energy.rate_hz = 2;
        analysis.energy.rms_dbfs = Some(-18.0);
        analysis.energy.sample_peak_dbfs = Some(-1.0);
        analysis.energy.integrated_lufs = Some(-14.0);
        analysis.energy.true_peak_dbtp = Some(-0.5);
        analysis.tonal.global_key = Some(crate::analysis::MusicalKey {
            tonic: 7,
            mode: crate::analysis::KeyMode::Minor,
            confidence: Confidence::clamped(0.9),
        });
        analysis.rhythm.meter_hypotheses = vec![MeterHypothesis {
            beats_per_bar: 4,
            downbeat_phase: 0,
            score: UnitInterval::clamped(0.8),
        }];
        analysis.structure.sections = vec![
            Section::new(0, 2, 0.8, [SectionLabel::Intro]),
            Section::new(6, 8, 0.9, [SectionLabel::Outro]),
        ];

        let legacy = analysis.as_v2_legacy_view();
        assert_eq!(legacy.vocal_activity, vec![64, 255]);
        assert_eq!(legacy.vocal_activity_confidences, vec![128, 255]);
        assert_eq!(legacy.vocal_activity_rate, 2);
        assert_eq!(legacy.energy_profile, vec![51, 204]);
        assert_eq!(legacy.energy_profile_rate, 2);
        assert_eq!(legacy.rms_dbfs, Some(-18.0));
        assert_eq!(legacy.sample_peak_dbfs, Some(-1.0));
        assert_eq!(legacy.integrated_lufs, Some(-14.0));
        assert_eq!(legacy.true_peak_dbtp, Some(-0.5));
        assert_eq!(legacy.musical_key.map(|key| key.tonic), Some(7));
        assert_eq!(legacy.first_downbeat, Some(Duration::from_secs(172)));
        assert_eq!(legacy.downbeat_confidence, 0.8);
        assert_eq!(legacy.intro_end, Some(Duration::from_secs(174)));
        assert_eq!(legacy.outro_start, Some(Duration::from_secs(178)));
        assert_eq!(legacy.intro_confidence, 0.8);
        assert_eq!(legacy.outro_confidence, 0.9);
    }

    #[test]
    fn soft_reliability_values_keep_a_physical_beatmatched_candidate() {
        for confidence in [0.68_f32, 0.69, 0.70, 0.71] {
            let (outgoing, incoming) = paired_edge_analyses(confidence);
            let planned = plan_transition_v2(&outgoing, &incoming, &config());

            assert!(
                planned.diagnostics.beatmatched_candidates > 0,
                "confidence={confidence} diagnostics={:?}",
                planned.diagnostics
            );
            assert!(
                planned
                    .candidates
                    .iter()
                    .any(|candidate| candidate.plan.kind == TransitionKind::BeatMatched),
                "confidence={confidence} candidates={:?}",
                planned.candidates
            );
        }
    }

    #[test]
    fn low_reliability_can_choose_crossfade_while_reporting_eligibility() {
        // Preserve enough observed events for a physical candidate, but use
        // zero kick confidence.  This should increase the soft BeatMatched
        // cost without turning it into a hard rejection.
        let outgoing_times = [
            172.000_f32,
            173.000,
            174.000,
            175.000,
            176.000,
            177.000,
            178.000,
            179.000,
        ];
        let incoming_times = [0.0_f32, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0];
        let outgoing = analysis_with_markers(&outgoing_times, 0.0, 60.0);
        let incoming = analysis_with_markers(&incoming_times, 0.0, 60.0);
        let mut high_target = config();
        high_target.min_beat_confidence = 0.99;

        let planned = plan_transition_v2(&outgoing, &incoming, &high_target);
        assert!(
            planned.diagnostics.beatmatched_candidates > 0,
            "{planned:?}"
        );
        assert_eq!(planned.plan.kind, TransitionKind::Crossfade, "{planned:?}");
        assert!(planned.diagnostics.eligible_but_crossfaded);
        assert!(planned.candidates.iter().any(|candidate| {
            candidate.plan.kind == TransitionKind::BeatMatched && candidate.hard_rejection.is_none()
        }));
    }

    #[test]
    fn no_rhythm_never_reports_a_crossfade_as_beatmatched_eligible() {
        let outgoing = TrackAnalysis::unanalyzed(Duration::from_secs(180));
        let incoming = TrackAnalysis::unanalyzed(Duration::from_secs(180));
        let planned = plan_transition_v2(&outgoing, &incoming, &config());

        assert_eq!(planned.diagnostics.beatmatched_candidates, 0);
        assert!(!planned.diagnostics.eligible_but_crossfaded);
        assert!(
            planned
                .diagnostics
                .has_reason(AutoMixV2Reason::NoUsableRhythmTimeline)
        );
    }

    #[test]
    fn malformed_event_times_fail_closed_without_panicking() {
        let mut outgoing = TrackAnalysis::unanalyzed(Duration::from_secs(180));
        outgoing.bpm = Some(60.0);
        outgoing.beat_markers = vec![Duration::from_secs(1), Duration::from_secs(1)];
        outgoing.beat_marker_confidences = vec![0.9, 0.9];
        let incoming = paired_edge_analyses(0.9).1;

        let result =
            std::panic::catch_unwind(|| beat_match_eligibility(&outgoing, &incoming, &config()));
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap().rejection,
            Some(BeatMatchRejection::InvalidBeatEventTimes)
        );
    }
}
