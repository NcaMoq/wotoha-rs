//! Continuous V2 strategy scoring.

use std::time::Duration;

use super::{
    TrackAnalysis, TransitionKind, TransitionPlan, evaluate_transition_quality_with_base_gains,
    transition_score_breakdown,
};
use crate::analysis::DjCue;
use crate::automix::reliability::pair_reliability;

use super::MAX_BEATMATCH_PHASE_ERROR;

/// Additive cost components for one transition candidate.
///
/// All fields are non-negative and finite for candidates returned by the V2
/// planner.  The fixed strategy bases make selection deterministic and leave
/// the legacy quality model available as a finite tie-breaker.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TransitionCostBreakdown {
    pub total: f32,
    pub strategy_base_cost: f32,
    pub tempo_stretch_cost: f32,
    pub phase_precision_cost: f32,
    pub structure_uncertainty_cost: f32,
    pub structure_alignment_cost: f32,
    pub rhythm_uncertainty_cost: f32,
    pub cue_suitability_cost: f32,
    pub blend_duration_cost: f32,
    pub legacy_quality_cost: f32,
}

impl TransitionCostBreakdown {
    pub const BEATMATCHED_BASE: f32 = 0.0;
    pub const CROSSFADE_BASE: f32 = 0.28;
    pub const GAPLESS_BASE: f32 = 0.55;
    /// Maximum penalty for a weakly supported outgoing/incoming cue pair.
    pub const MAX_CUE_SUITABILITY_COST: f32 = 0.24;
    /// Each cue's role score contributes half of its suitability signal.
    pub const CUE_ROLE_SCORE_WEIGHT: f32 = 0.5;
    /// Each cue's importance contributes half of its suitability signal.
    pub const CUE_IMPORTANCE_SCORE_WEIGHT: f32 = 0.5;

    pub fn zero(kind: TransitionKind) -> Self {
        let strategy_base_cost = Self::strategy_base(kind);
        Self {
            total: strategy_base_cost,
            strategy_base_cost,
            tempo_stretch_cost: 0.0,
            phase_precision_cost: 0.0,
            structure_uncertainty_cost: 0.0,
            structure_alignment_cost: 0.0,
            rhythm_uncertainty_cost: 0.0,
            cue_suitability_cost: 0.0,
            blend_duration_cost: 0.0,
            legacy_quality_cost: 0.0,
        }
    }

    pub const fn strategy_base(kind: TransitionKind) -> f32 {
        match kind {
            TransitionKind::BeatMatched => Self::BEATMATCHED_BASE,
            TransitionKind::Crossfade => Self::CROSSFADE_BASE,
            TransitionKind::Gapless => Self::GAPLESS_BASE,
        }
    }

    /// Explicitly named accessor matching the public component field.  The
    /// shorter `strategy_base` method remains as a source-compatible alias
    /// for early V2 integrations.
    pub const fn strategy_base_cost(kind: TransitionKind) -> f32 {
        Self::strategy_base(kind)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn from_components(
        kind: TransitionKind,
        tempo_adjustment: f32,
        max_tempo_adjustment: f32,
        phase_error: Option<Duration>,
        reliability: f32,
        target_confidence: f32,
        structure_quality: Option<f32>,
        legacy_quality: f32,
    ) -> Self {
        let max_adjustment = max_tempo_adjustment.abs().max(f32::EPSILON);
        let normalized = (tempo_adjustment.abs() / max_adjustment).clamp(0.0, 1.0);
        let tempo_stretch_cost = if kind == TransitionKind::BeatMatched {
            0.30 * normalized * normalized
        } else {
            0.0
        };
        let phase_precision_cost = if kind == TransitionKind::BeatMatched {
            phase_error
                .map(|error| {
                    let normalized = error.as_secs_f32() / MAX_BEATMATCH_PHASE_ERROR.as_secs_f32();
                    0.40 * normalized.clamp(0.0, 1.0).powi(2)
                })
                .unwrap_or(0.40)
        } else {
            0.0
        };
        let structure_uncertainty_cost = if kind == TransitionKind::BeatMatched {
            let quality = structure_quality
                .filter(|quality| quality.is_finite())
                .map(|quality| quality.clamp(0.0, 1.0));
            // Structure is a soft preference.  In particular, unavailable or
            // mismatching phrase metadata never turns into a hard rejection.
            quality.map_or(0.15, |quality| 0.15 * (1.0 - quality))
        } else {
            0.0
        };
        let rhythm_uncertainty_cost = if kind == TransitionKind::BeatMatched {
            rhythm_uncertainty_cost(reliability, target_confidence)
        } else {
            0.0
        };
        let legacy_quality_cost = if legacy_quality.is_finite() {
            legacy_quality.max(0.0)
        } else {
            1.0
        };
        let strategy_base_cost = Self::strategy_base(kind);
        let total = strategy_base_cost
            + tempo_stretch_cost
            + phase_precision_cost
            + structure_uncertainty_cost
            + rhythm_uncertainty_cost
            + legacy_quality_cost;
        Self {
            total: finite_cost(total),
            strategy_base_cost,
            tempo_stretch_cost,
            phase_precision_cost,
            structure_uncertainty_cost,
            structure_alignment_cost: 0.0,
            rhythm_uncertainty_cost,
            cue_suitability_cost: 0.0,
            blend_duration_cost: 0.0,
            legacy_quality_cost,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn for_plan(
        outgoing: &TrackAnalysis,
        incoming: &TrackAnalysis,
        plan: &TransitionPlan,
        tempo_adjustment: f32,
        max_tempo_adjustment: f32,
        phase_error: Option<Duration>,
        reliability: f32,
        target_confidence: f32,
    ) -> Self {
        Self::for_plan_with_base_gains(
            outgoing,
            incoming,
            plan,
            tempo_adjustment,
            max_tempo_adjustment,
            phase_error,
            reliability,
            target_confidence,
            1.0,
            1.0,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn for_plan_with_base_gains(
        outgoing: &TrackAnalysis,
        incoming: &TrackAnalysis,
        plan: &TransitionPlan,
        tempo_adjustment: f32,
        max_tempo_adjustment: f32,
        phase_error: Option<Duration>,
        reliability: f32,
        target_confidence: f32,
        outgoing_base_gain: f32,
        incoming_base_gain: f32,
    ) -> Self {
        let quality = evaluate_transition_quality_with_base_gains(
            outgoing,
            incoming,
            plan,
            outgoing_base_gain,
            incoming_base_gain,
        );
        // The existing score is useful as a preference, but V2 decides hard
        // eligibility separately.  A missing breakdown is simply neutral.
        let legacy_quality = transition_score_breakdown(&quality)
            .map(|breakdown| breakdown.total)
            .unwrap_or(0.0);
        let structure_quality = quality.phrase_boundary_bars.map(|bars| match bars {
            16 => 1.0,
            8 => 0.73,
            4 => 0.33,
            _ => 0.0,
        });
        Self::from_components(
            plan.kind,
            tempo_adjustment,
            max_tempo_adjustment,
            phase_error,
            reliability,
            target_confidence,
            structure_quality,
            legacy_quality,
        )
    }

    pub fn with_pair_reliability(
        outgoing: &TrackAnalysis,
        incoming: &TrackAnalysis,
        plan: &TransitionPlan,
        tempo_adjustment: f32,
        max_tempo_adjustment: f32,
        phase_error: Option<Duration>,
        target_confidence: f32,
    ) -> Self {
        Self::for_plan(
            outgoing,
            incoming,
            plan,
            tempo_adjustment,
            max_tempo_adjustment,
            phase_error,
            pair_reliability(
                super::reliability::compute_reliability(outgoing),
                super::reliability::compute_reliability(incoming),
            ),
            target_confidence,
        )
    }

    /// Add the soft suitability penalty for the cues anchoring this plan.
    /// Cue strength affects ranking only; it never makes a physically valid
    /// plan ineligible.
    pub fn with_cue_suitability(mut self, outgoing: &DjCue, incoming: &DjCue) -> Self {
        self.cue_suitability_cost = cue_suitability_cost(outgoing, incoming);
        self.total = finite_cost(self.total + self.cue_suitability_cost);
        self
    }

    /// Add a bounded structure-alignment preference. Structure evidence is
    /// intentionally a ranking signal and never changes hard eligibility.
    pub fn with_structure_alignment(mut self, cost: f32) -> Self {
        self.structure_alignment_cost = finite_cost(cost);
        self.total = finite_cost(self.total + self.structure_alignment_cost);
        self
    }

    /// Prefer the configured phrase-sized blend while keeping shorter and
    /// longer physically valid families available as soft alternatives.
    pub fn with_blend_duration(
        mut self,
        beats: usize,
        preferred_beats: usize,
        min_beats: usize,
        max_beats: usize,
    ) -> Self {
        let preferred = preferred_beats.max(1) as f32;
        let span = max_beats.saturating_sub(min_beats).max(1) as f32;
        let distance = (beats as f32 - preferred).abs() / span.max(preferred);
        self.blend_duration_cost = (0.12 * distance.min(1.0)).max(0.0);
        self.total = finite_cost(self.total + self.blend_duration_cost);
        self
    }
}

/// Return a bounded soft cost for an outgoing/incoming cue pair.
///
/// Role scores and general importance are both considered for both tracks.
/// The maximum penalty is deliberately modest so cue evidence steers ties but
/// does not overwhelm rhythm, phase, or strategy costs.
pub fn cue_suitability_cost(outgoing_mix_out: &DjCue, incoming_mix_in: &DjCue) -> f32 {
    TransitionCostBreakdown::MAX_CUE_SUITABILITY_COST
        * (1.0 - cue_suitability_score(outgoing_mix_out, incoming_mix_in))
}

/// Combined, bounded suitability score used to explain the cue cost.
pub fn cue_suitability_score(outgoing_mix_out: &DjCue, incoming_mix_in: &DjCue) -> f32 {
    let outgoing_score = outgoing_mix_out.mix_out.get()
        * TransitionCostBreakdown::CUE_ROLE_SCORE_WEIGHT
        + outgoing_mix_out.importance.get() * TransitionCostBreakdown::CUE_IMPORTANCE_SCORE_WEIGHT;
    let incoming_score = incoming_mix_in.mix_in.get()
        * TransitionCostBreakdown::CUE_ROLE_SCORE_WEIGHT
        + incoming_mix_in.importance.get() * TransitionCostBreakdown::CUE_IMPORTANCE_SCORE_WEIGHT;
    (outgoing_score.clamp(0.0, 1.0) * incoming_score.clamp(0.0, 1.0))
        .sqrt()
        .clamp(0.0, 1.0)
}

pub fn rhythm_uncertainty_cost(reliability: f32, min_beat_confidence: f32) -> f32 {
    let target = if min_beat_confidence.is_finite() {
        min_beat_confidence.clamp(0.05, 1.0)
    } else {
        0.05
    };
    let reliability = if reliability.is_finite() {
        reliability.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let deficit = (target - reliability).max(0.0) / target;
    finite_cost(0.65 * deficit + 0.15 * (1.0 - reliability))
}

fn finite_cost(value: f32) -> f32 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        f32::MAX
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reliability_penalty_is_finite_and_continuous_around_target() {
        let costs = [0.68_f32, 0.69, 0.70, 0.71]
            .into_iter()
            .map(|reliability| rhythm_uncertainty_cost(reliability, 0.70))
            .collect::<Vec<_>>();

        assert!(costs.iter().all(|cost| cost.is_finite() && *cost >= 0.0));
        assert!(costs.windows(2).all(|window| window[1] <= window[0]));
        assert!(
            costs
                .windows(2)
                .all(|window| (window[1] - window[0]).abs() < 0.02)
        );
    }

    #[test]
    fn cue_suitability_uses_both_role_and_importance_as_a_soft_cost() {
        let outgoing = DjCue::from_bool(4, 1.0, false, true);
        let incoming = DjCue::from_bool(8, 1.0, true, false);
        let strong = cue_suitability_cost(&outgoing, &incoming);
        assert_eq!(strong, 0.0);

        let mut weak_outgoing = outgoing;
        weak_outgoing.importance = crate::analysis::UnitInterval::ZERO;
        weak_outgoing.mix_out = crate::analysis::UnitInterval::clamped(0.1);
        let mut weak_incoming = incoming;
        weak_incoming.importance = crate::analysis::UnitInterval::ZERO;
        weak_incoming.mix_in = crate::analysis::UnitInterval::clamped(0.1);
        let weak = cue_suitability_cost(&weak_outgoing, &weak_incoming);
        assert!(weak > strong);
        assert!(weak <= TransitionCostBreakdown::MAX_CUE_SUITABILITY_COST);
    }

    #[test]
    fn invalid_reliability_inputs_fail_closed_to_finite_costs() {
        for reliability in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let cost = rhythm_uncertainty_cost(reliability, 0.70);
            assert!(cost.is_finite());
            assert!(cost >= 0.0);
        }
        let cost = rhythm_uncertainty_cost(0.70, f32::NAN);
        assert!(cost.is_finite());
    }
}
