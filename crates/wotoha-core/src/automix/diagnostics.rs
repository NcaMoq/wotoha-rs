//! Stable diagnostics for the additive AutoMix V2 planner.

use super::{TransitionKind, scoring::TransitionCostBreakdown};

/// Why a V2 candidate was rejected or why a lower-cost strategy was selected.
///
/// The legacy `AutoMixBeatMatchDecision` remains unchanged.  These reasons are
/// intentionally V2-specific so callers can migrate diagnostics without
/// changing existing runtime control flow.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AutoMixV2Reason {
    BeatMatchedSelected,
    BeatMatchAvailableButCrossfadePreferred,
    BeatMatchAvailableButGaplessPreferred,
    NoUsableRhythmTimeline,
    InvalidBeatEventTimes,
    NotEnoughBeatPairs,
    PhaseDriftTooLarge,
    NoCompatibleTempoHypothesis,
    TempoAdjustmentExceeded,
    InvalidDspParameters,
    PhysicalWindowUnavailable,
    CrossfadeSelected,
    GaplessSelected,
    QualityGuardRejected,
    Disabled,
}

/// Rejection details are kept separate from soft planner reasons.  The list
/// is bounded by [`PlannerDiagnostics::MAX_HARD_REJECTIONS`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CandidateRejection {
    NoUsableRhythmTimeline,
    InvalidBeatEventTimes,
    NotEnoughBeatPairs,
    PhaseDriftTooLarge,
    NoCompatibleTempoHypothesis,
    TempoAdjustmentExceeded,
    InvalidDspParameters,
    PhysicalWindowUnavailable,
    QualityGuardRejected,
}

impl CandidateRejection {
    pub const fn reason(self) -> AutoMixV2Reason {
        match self {
            Self::NoUsableRhythmTimeline => AutoMixV2Reason::NoUsableRhythmTimeline,
            Self::InvalidBeatEventTimes => AutoMixV2Reason::InvalidBeatEventTimes,
            Self::NotEnoughBeatPairs => AutoMixV2Reason::NotEnoughBeatPairs,
            Self::PhaseDriftTooLarge => AutoMixV2Reason::PhaseDriftTooLarge,
            Self::NoCompatibleTempoHypothesis => AutoMixV2Reason::NoCompatibleTempoHypothesis,
            Self::TempoAdjustmentExceeded => AutoMixV2Reason::TempoAdjustmentExceeded,
            Self::InvalidDspParameters => AutoMixV2Reason::InvalidDspParameters,
            Self::PhysicalWindowUnavailable => AutoMixV2Reason::PhysicalWindowUnavailable,
            Self::QualityGuardRejected => AutoMixV2Reason::QualityGuardRejected,
        }
    }
}

impl AutoMixV2Reason {
    pub const fn is_hard_rejection(self) -> bool {
        matches!(
            self,
            Self::NoUsableRhythmTimeline
                | Self::InvalidBeatEventTimes
                | Self::NotEnoughBeatPairs
                | Self::PhaseDriftTooLarge
                | Self::NoCompatibleTempoHypothesis
                | Self::TempoAdjustmentExceeded
                | Self::InvalidDspParameters
                | Self::PhysicalWindowUnavailable
                | Self::QualityGuardRejected
        )
    }
}

/// Bounded planner diagnostics.  Candidate counts are retained separately so
/// callers can distinguish unavailable strategies from strategies that lost a
/// cost comparison.
#[derive(Clone, Debug, PartialEq)]
pub struct PlannerDiagnostics {
    pub beatmatched_candidates: usize,
    pub crossfade_candidates: usize,
    pub gapless_candidates: usize,
    pub hard_rejections: Vec<CandidateRejection>,
    pub selected_kind: TransitionKind,
    pub selected_cost: TransitionCostBreakdown,
    pub eligible_but_crossfaded: bool,
}

impl PlannerDiagnostics {
    pub const MAX_HARD_REJECTIONS: usize = 16;

    pub fn new(selected_kind: TransitionKind, selected_cost: TransitionCostBreakdown) -> Self {
        Self {
            beatmatched_candidates: 0,
            crossfade_candidates: 0,
            gapless_candidates: 0,
            hard_rejections: Vec::new(),
            selected_kind,
            selected_cost,
            eligible_but_crossfaded: false,
        }
    }

    pub fn add_rejection(&mut self, rejection: CandidateRejection) {
        if self.hard_rejections.len() < Self::MAX_HARD_REJECTIONS
            && !self.hard_rejections.contains(&rejection)
        {
            self.hard_rejections.push(rejection);
        }
    }

    /// Compatibility adapter for code that recorded `AutoMixV2Reason`
    /// directly.  Soft reasons are intentionally not stored as hard
    /// rejections.
    pub fn add_reason(&mut self, reason: AutoMixV2Reason) {
        let rejection = match reason {
            AutoMixV2Reason::NoUsableRhythmTimeline => {
                Some(CandidateRejection::NoUsableRhythmTimeline)
            }
            AutoMixV2Reason::InvalidBeatEventTimes => {
                Some(CandidateRejection::InvalidBeatEventTimes)
            }
            AutoMixV2Reason::NotEnoughBeatPairs => Some(CandidateRejection::NotEnoughBeatPairs),
            AutoMixV2Reason::PhaseDriftTooLarge => Some(CandidateRejection::PhaseDriftTooLarge),
            AutoMixV2Reason::NoCompatibleTempoHypothesis => {
                Some(CandidateRejection::NoCompatibleTempoHypothesis)
            }
            AutoMixV2Reason::TempoAdjustmentExceeded => {
                Some(CandidateRejection::TempoAdjustmentExceeded)
            }
            AutoMixV2Reason::InvalidDspParameters => Some(CandidateRejection::InvalidDspParameters),
            AutoMixV2Reason::PhysicalWindowUnavailable => {
                Some(CandidateRejection::PhysicalWindowUnavailable)
            }
            AutoMixV2Reason::QualityGuardRejected => Some(CandidateRejection::QualityGuardRejected),
            _ => None,
        };
        if let Some(rejection) = rejection {
            self.add_rejection(rejection);
        }
    }

    pub fn has_rejection(&self, rejection: CandidateRejection) -> bool {
        self.hard_rejections.contains(&rejection)
    }

    pub fn has_reason(&self, reason: AutoMixV2Reason) -> bool {
        let Some(rejection) = (match reason {
            AutoMixV2Reason::NoUsableRhythmTimeline => {
                Some(CandidateRejection::NoUsableRhythmTimeline)
            }
            AutoMixV2Reason::InvalidBeatEventTimes => {
                Some(CandidateRejection::InvalidBeatEventTimes)
            }
            AutoMixV2Reason::NotEnoughBeatPairs => Some(CandidateRejection::NotEnoughBeatPairs),
            AutoMixV2Reason::PhaseDriftTooLarge => Some(CandidateRejection::PhaseDriftTooLarge),
            AutoMixV2Reason::NoCompatibleTempoHypothesis => {
                Some(CandidateRejection::NoCompatibleTempoHypothesis)
            }
            AutoMixV2Reason::TempoAdjustmentExceeded => {
                Some(CandidateRejection::TempoAdjustmentExceeded)
            }
            AutoMixV2Reason::InvalidDspParameters => Some(CandidateRejection::InvalidDspParameters),
            AutoMixV2Reason::PhysicalWindowUnavailable => {
                Some(CandidateRejection::PhysicalWindowUnavailable)
            }
            AutoMixV2Reason::QualityGuardRejected => Some(CandidateRejection::QualityGuardRejected),
            _ => None,
        }) else {
            return false;
        };
        self.has_rejection(rejection)
    }

    /// V2's low-confidence behavior is a choice among candidates, not a hard
    /// rejection.  This flag is true only when BeatMatched was eligible and a
    /// Crossfade won the total-cost comparison.
    pub fn set_selection(
        &mut self,
        selected_kind: TransitionKind,
        selected_cost: TransitionCostBreakdown,
        beat_matched_eligible: bool,
    ) {
        self.selected_kind = selected_kind;
        self.selected_cost = selected_cost;
        self.eligible_but_crossfaded =
            beat_matched_eligible && selected_kind == TransitionKind::Crossfade;
    }

    pub fn add_candidate(&mut self, kind: TransitionKind) {
        match kind {
            TransitionKind::BeatMatched => self.beatmatched_candidates += 1,
            TransitionKind::Crossfade => self.crossfade_candidates += 1,
            TransitionKind::Gapless => self.gapless_candidates += 1,
        }
    }
}

impl Default for PlannerDiagnostics {
    fn default() -> Self {
        Self::new(
            TransitionKind::Gapless,
            TransitionCostBreakdown::zero(TransitionKind::Gapless),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn low_reliability_can_select_crossfade_without_claiming_ineligibility() {
        let cost = TransitionCostBreakdown::zero(TransitionKind::Crossfade);
        let mut diagnostics = PlannerDiagnostics::new(TransitionKind::Crossfade, cost);
        diagnostics.set_selection(TransitionKind::Crossfade, cost, true);

        assert!(diagnostics.eligible_but_crossfaded);
        assert_eq!(diagnostics.selected_kind, TransitionKind::Crossfade);
    }

    #[test]
    fn missing_rhythm_is_distinguished_from_soft_crossfade_preference() {
        let mut missing = PlannerDiagnostics::default();
        missing.add_reason(AutoMixV2Reason::NoUsableRhythmTimeline);
        assert!(!missing.eligible_but_crossfaded);
        assert!(missing.has_reason(AutoMixV2Reason::NoUsableRhythmTimeline));
        assert!(AutoMixV2Reason::NoUsableRhythmTimeline.is_hard_rejection());

        let mut soft = PlannerDiagnostics::default();
        soft.add_reason(AutoMixV2Reason::BeatMatchAvailableButCrossfadePreferred);
        let cost = TransitionCostBreakdown::zero(TransitionKind::Crossfade);
        soft.set_selection(TransitionKind::Crossfade, cost, true);
        assert!(soft.eligible_but_crossfaded);
        assert!(!AutoMixV2Reason::BeatMatchAvailableButCrossfadePreferred.is_hard_rejection());
    }

    #[test]
    fn diagnostics_bound_duplicate_reason_growth() {
        let mut diagnostics = PlannerDiagnostics::default();
        for _ in 0..64 {
            diagnostics.add_reason(AutoMixV2Reason::InvalidDspParameters);
        }
        assert_eq!(diagnostics.hard_rejections.len(), 1);
    }
}
