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
    TransitionKind, TransitionPlan, evaluate_transition_quality, harmonic_compatibility,
    plan_transition_timing,
};

const MIN_V2_BEAT_PAIRS: usize = 3;
const MAX_CANDIDATES: usize = 3;

use crate::analysis::{TempoRelation, TrackAnalysisV2, UnitInterval};

/// Adapter boundary accepted by the V2 planner.  Keeping this trait generic
/// lets existing V1 callers use the new cost/diagnostic API while versioned
/// callers retain the richer `TrackAnalysisV2` beat-event timeline.
pub trait V2AnalysisInput {
    fn as_v2_legacy_view(&self) -> TrackAnalysis;

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

        if let Some(meter) = self
            .rhythm
            .meter_hypotheses
            .iter()
            .filter(|meter| meter.validate())
            .max_by(|left, right| left.score.get().total_cmp(&right.score.get()))
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
        Some(Self {
            outgoing,
            incoming,
            ratio,
            normalized_adjustment: (ratio - 1.0).abs(),
            weight,
            // A lower stretch is preferred; when stretch is equal, stronger
            // hypotheses win.  This is used only as a deterministic ordering
            // signal; the planner's total cost remains authoritative.
            cost: (ratio - 1.0).abs() + (1.0 - weight) * 0.001,
        })
    }

    pub fn within_adjustment(self, max_tempo_adjustment: f32) -> bool {
        max_tempo_adjustment.is_finite()
            && max_tempo_adjustment >= 0.0
            && self.ratio.is_finite()
            && self.ratio > 0.0
            && (self.ratio - 1.0).abs() <= max_tempo_adjustment
    }
}

pub fn cross_product_tempo_hypotheses(
    outgoing: &[TempoHypothesis],
    incoming: &[TempoHypothesis],
) -> Vec<TempoHypothesisPair> {
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
            .then_with(|| right.weight.total_cmp(&left.weight))
    });
    pairs
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
            let left_stretch = (left.ratio - 1.0).abs();
            let right_stretch = (right.ratio - 1.0).abs();
            left_stretch
                .total_cmp(&right_stretch)
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
    let outgoing_timeline = outgoing.rhythm_timeline();
    let incoming_timeline = incoming.rhythm_timeline();
    let outgoing_hypotheses = outgoing.tempo_hypotheses();
    let incoming_hypotheses = incoming.tempo_hypotheses();
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
    let planned = plan_transition_v2(outgoing, incoming, config);
    let outgoing = outgoing.as_v2_legacy_view();
    let incoming = incoming.as_v2_legacy_view();
    let raw_quality = evaluate_transition_quality(&outgoing, &incoming, &planned.plan);
    let quality = v2_quality_report(&raw_quality, planned.plan.kind);
    if !quality_has_v2_hard_issue(&raw_quality, planned.plan.kind) {
        return V2GuardedTransitionPlan {
            plan: planned.plan,
            cost: planned.cost,
            quality,
            diagnostics: planned.diagnostics,
            rejected_plan: None,
            rejected_quality: None,
        };
    }

    // Re-run the bounded candidate list in ascending cost and retain the first
    // candidate passing V2's hard guard.  Soft quality concerns remain costs.
    for candidate in &planned.candidates {
        let raw_candidate_quality =
            evaluate_transition_quality(&outgoing, &incoming, &candidate.plan);
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
        &evaluate_transition_quality(&outgoing, &incoming, &gapless),
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

fn plan_transition_v2_internal_with_context(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    config: &AutoMixConfig,
    outgoing_timeline: &BeatTimeline,
    incoming_timeline: &BeatTimeline,
    outgoing_hypotheses: &[TempoHypothesis],
    incoming_hypotheses: &[TempoHypothesis],
) -> (TransitionPlanV2, BeatMatchEligibility) {
    let gapless = gapless_plan(outgoing, incoming);
    let pair_reliability = pair_reliability(
        reliability_for_timeline(outgoing_timeline).reliability,
        reliability_for_timeline(incoming_timeline).reliability,
    );
    let mut candidates = Vec::with_capacity(MAX_CANDIDATES);
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
    if let Some(reason) = eligibility.rejection {
        diagnostics.add_reason(reason.reason());
    }

    if config.enabled
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
        let cost = TransitionCostBreakdown::for_plan(
            outgoing,
            incoming,
            &plan,
            (tempo.ratio - 1.0).abs(),
            config.max_tempo_adjustment,
            eligibility.phase_error,
            local_pair_reliability,
            config.min_beat_confidence,
        );
        let quality = evaluate_transition_quality(outgoing, incoming, &plan);
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
            });
        }
    }

    let crossfade = crossfade_plan(outgoing, incoming, config);
    if crossfade.kind == TransitionKind::Crossfade {
        let quality = evaluate_transition_quality(outgoing, incoming, &crossfade);
        let hard_rejection = quality_rejection(&quality, crossfade.kind);
        if let Some(reason) = hard_rejection {
            diagnostics.add_reason(reason.reason());
        } else {
            diagnostics.add_candidate(TransitionKind::Crossfade);
            let cost = TransitionCostBreakdown::for_plan(
                outgoing,
                incoming,
                &crossfade,
                0.0,
                config.max_tempo_adjustment,
                None,
                pair_reliability,
                config.min_beat_confidence,
            );
            candidates.push(TransitionCandidate {
                plan: crossfade,
                cost,
                beat_eligibility: Some(eligibility.clone()),
                hard_rejection: None,
            });
        }
    } else {
        diagnostics.add_reason(AutoMixV2Reason::PhysicalWindowUnavailable);
    }

    let gapless_cost = TransitionCostBreakdown::for_plan(
        outgoing,
        incoming,
        &gapless,
        0.0,
        config.max_tempo_adjustment,
        None,
        pair_reliability,
        config.min_beat_confidence,
    );
    candidates.push(TransitionCandidate {
        plan: gapless,
        cost: gapless_cost,
        beat_eligibility: Some(eligibility.clone()),
        hard_rejection: None,
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
        });
    let beat_eligible = eligibility.eligible
        && candidates.iter().any(|candidate| {
            candidate.plan.kind == TransitionKind::BeatMatched && candidate.hard_rejection.is_none()
        });
    diagnostics.set_selection(selected.plan.kind, selected.cost, beat_eligible);
    if selected.plan.kind == TransitionKind::BeatMatched {
        diagnostics.add_reason(AutoMixV2Reason::BeatMatchedSelected);
    } else if beat_eligible && selected.plan.kind == TransitionKind::Crossfade {
        diagnostics.add_reason(AutoMixV2Reason::BeatMatchAvailableButCrossfadePreferred);
        diagnostics.add_reason(AutoMixV2Reason::CrossfadeSelected);
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
        | AutoMixQualityIssue::OutgoingOverlapMissesAudibleEnd { .. }
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
        | AutoMixQualityIssue::OutgoingOverlapMissesAudibleEnd { .. }
        | AutoMixQualityIssue::IncomingOverlapExceedsAudibleEnd { .. }
        | AutoMixQualityIssue::BeatPhaseDriftTooLarge { .. }
        | AutoMixQualityIssue::BeatHandoffPhaseDriftTooLarge { .. }
        | AutoMixQualityIssue::DualVocalOverlapTooHigh { .. }
        | AutoMixQualityIssue::MixEnergyDipTooDeep { .. } => true,
        AutoMixQualityIssue::BeatPhaseUnverified
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
        BeatEvent, Confidence, MeterHypothesis, ModelScore, Section, SectionLabel, Support,
        TempoHypothesis, TempoRelation, TrackAnalysisV2, UnitInterval,
    };

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
