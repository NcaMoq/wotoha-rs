//! Reliability for the observed rhythm timelines used by AutoMix V2.
//!
//! A timeline is evidence, not a request to reconstruct a grid.  In
//! particular, this module never creates beat events from BPM.  The legacy
//! [`TrackAnalysis`](super::TrackAnalysis) representation is adapted here by
//! copying its observed marker times.

use std::time::Duration;

use super::TrackAnalysis;
use crate::analysis::{BeatEvent, RhythmAnalysis, TrackAnalysisV2};

/// Internal adapter view of a domain [`BeatEvent`].  The domain event remains
/// the source of truth; this view only lets the legacy TrackAnalysis adapter
/// expose equivalent evidence without defining a second BeatEvent type.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TimelineEvent {
    pub time: Duration,
    pub timing_confidence: Option<f32>,
    pub beat_evidence: Option<f32>,
    pub generic_onset: Option<f32>,
}

/// A source-time ordered sequence of observed beat events.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BeatTimeline {
    pub events: Vec<TimelineEvent>,
}

impl BeatTimeline {
    pub fn new(events: Vec<TimelineEvent>) -> Self {
        Self { events }
    }

    pub fn from_times(times: impl IntoIterator<Item = Duration>) -> Self {
        Self::new(
            times
                .into_iter()
                .map(|time| TimelineEvent {
                    time,
                    timing_confidence: None,
                    beat_evidence: None,
                    generic_onset: None,
                })
                .collect(),
        )
    }

    pub fn from_beat_events(events: &[BeatEvent]) -> Self {
        timeline_from_beat_events(events)
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// True only when times are finite, strictly increasing and confidence
    /// values (when present) are finite and bounded.
    pub fn has_strict_times(&self) -> bool {
        self.events.windows(2).all(|pair| {
            pair[1].time > pair[0].time
                && pair[0]
                    .timing_confidence
                    .is_none_or(|value| value.is_finite() && (0.0..=1.0).contains(&value))
                && pair[0]
                    .beat_evidence
                    .is_none_or(|value| value.is_finite() && (0.0..=1.0).contains(&value))
                && pair[0]
                    .generic_onset
                    .is_none_or(|value| value.is_finite() && (0.0..=1.0).contains(&value))
        }) && self.events.last().is_none_or(|event| {
            event
                .timing_confidence
                .is_none_or(|value| value.is_finite() && (0.0..=1.0).contains(&value))
                && event
                    .beat_evidence
                    .is_none_or(|value| value.is_finite() && (0.0..=1.0).contains(&value))
                && event
                    .generic_onset
                    .is_none_or(|value| value.is_finite() && (0.0..=1.0).contains(&value))
        })
    }

    pub fn between(
        &self,
        start: Duration,
        end: Duration,
    ) -> impl Iterator<Item = TimelineEvent> + '_ {
        self.events
            .iter()
            .copied()
            .filter(move |event| event.time >= start && event.time <= end)
    }
}

/// Components used to explain a track's rhythm reliability.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ReliabilityBreakdown {
    pub reliability: f32,
    pub timing: f32,
    pub interval_continuity: f32,
    pub beat_evidence: f32,
    pub generic_onset: Option<f32>,
}

impl ReliabilityBreakdown {
    pub const TIMING_WEIGHT: f32 = 0.45;
    pub const INTERVAL_CONTINUITY_WEIGHT: f32 = 0.25;
    pub const BEAT_EVIDENCE_WEIGHT: f32 = 0.20;
    pub const GENERIC_ONSET_WEIGHT: f32 = 0.10;

    pub const fn unavailable() -> Self {
        Self {
            reliability: 0.0,
            timing: 0.0,
            interval_continuity: 0.0,
            beat_evidence: 0.0,
            generic_onset: None,
        }
    }
}

/// Convert the currently persisted marker representation to timeline truth.
/// No marker is generated from `bpm` or `first_beat`.
pub fn timeline_from_analysis(analysis: &TrackAnalysis) -> BeatTimeline {
    BeatTimeline::new(
        analysis
            .beat_markers
            .iter()
            .copied()
            .enumerate()
            .map(|(index, time)| TimelineEvent {
                time,
                // V1 markers carry kick/onset evidence, not an independent
                // timing-confidence score.  Keep timing unknown so the
                // reliability aggregate uses its neutral value rather than
                // manufacturing certainty from the legacy representation.
                timing_confidence: None,
                beat_evidence: analysis.beat_marker_confidences.get(index).copied(),
                generic_onset: None,
            })
            .collect(),
    )
}

/// Adapt the versioned rhythm domain while preserving every observed event
/// time. BPM and phrase priors are never used to synthesize missing events.
pub fn timeline_from_rhythm(analysis: &RhythmAnalysis) -> BeatTimeline {
    BeatTimeline::new(
        analysis
            .beats
            .iter()
            .map(|event| TimelineEvent {
                time: event.time,
                timing_confidence: Some(event.timing_confidence.get()),
                beat_evidence: event.beat_model_score.map(|score| score.get()),
                generic_onset: event.onset_support.map(|support| support.get()),
            })
            .collect(),
    )
}

pub fn timeline_from_beat_events(events: &[BeatEvent]) -> BeatTimeline {
    BeatTimeline::new(
        events
            .iter()
            .map(|event| TimelineEvent {
                time: event.time,
                timing_confidence: Some(event.timing_confidence.get()),
                beat_evidence: event.beat_model_score.map(|score| score.get()),
                generic_onset: event.onset_support.map(|support| support.get()),
            })
            .collect(),
    )
}

pub fn reliability_for_beat_events(events: &[BeatEvent]) -> ReliabilityBreakdown {
    reliability_for_timeline(&timeline_from_beat_events(events))
}

pub fn timeline_from_track_analysis_v2(analysis: &TrackAnalysisV2) -> BeatTimeline {
    timeline_from_rhythm(&analysis.rhythm)
}

/// Calculate reliability from observed events.
pub fn reliability_for_timeline(timeline: &BeatTimeline) -> ReliabilityBreakdown {
    if timeline.events.len() < 2 || !timeline.has_strict_times() {
        return ReliabilityBreakdown::unavailable();
    }

    let mut timing_values = timeline
        .events
        .iter()
        .filter_map(|event| event.timing_confidence)
        .filter(|value| value.is_finite() && (0.0..=1.0).contains(value))
        .collect::<Vec<_>>();
    // Unknown timing confidence is neutral, not zero.  If confidence exists,
    // its aggregate contributes to the timing component independently of
    // low-frequency/onset support.
    let timing = robust_median(&mut timing_values).unwrap_or(0.5);
    let intervals = timeline
        .events
        .windows(2)
        .map(|pair| pair[1].time.as_secs_f64() - pair[0].time.as_secs_f64())
        .filter(|interval| interval.is_finite() && *interval > 0.0)
        .collect::<Vec<_>>();
    let interval_continuity = if intervals.is_empty() {
        0.0
    } else {
        let mean = intervals.iter().sum::<f64>() / intervals.len() as f64;
        let deviation = intervals
            .iter()
            .map(|interval| (*interval - mean).abs())
            .sum::<f64>()
            / intervals.len() as f64;
        (1.0 - deviation / mean.max(f64::EPSILON)).clamp(0.0, 1.0) as f32
    };

    let mut known_confidences = timeline
        .events
        .iter()
        .filter_map(|event| event.beat_evidence)
        .filter(|value| value.is_finite() && (0.0..=1.0).contains(value))
        .collect::<Vec<_>>();
    // Missing marker confidence is unknown, rather than evidence against the
    // timeline.  A neutral value lets timing and continuity carry the result.
    let beat_evidence = robust_median(&mut known_confidences).unwrap_or(0.5);

    let mut onset_values = timeline
        .events
        .iter()
        .filter_map(|event| event.generic_onset)
        .filter(|value| value.is_finite() && (0.0..=1.0).contains(value))
        .collect::<Vec<_>>();
    let generic_onset = robust_median(&mut onset_values);
    // An absent generic-onset observation is neutral evidence, not a zero.
    // Keep the published weights exact even when that optional component is
    // unavailable rather than silently renormalizing the other components.
    let onset_for_weight = generic_onset.unwrap_or(0.5);
    let weighted = timing * ReliabilityBreakdown::TIMING_WEIGHT
        + interval_continuity * ReliabilityBreakdown::INTERVAL_CONTINUITY_WEIGHT
        + beat_evidence * ReliabilityBreakdown::BEAT_EVIDENCE_WEIGHT
        + onset_for_weight * ReliabilityBreakdown::GENERIC_ONSET_WEIGHT;
    let weight = ReliabilityBreakdown::TIMING_WEIGHT
        + ReliabilityBreakdown::INTERVAL_CONTINUITY_WEIGHT
        + ReliabilityBreakdown::BEAT_EVIDENCE_WEIGHT
        + ReliabilityBreakdown::GENERIC_ONSET_WEIGHT;
    ReliabilityBreakdown {
        reliability: (weighted / weight.max(f32::EPSILON)).clamp(0.0, 1.0),
        timing,
        interval_continuity,
        beat_evidence,
        generic_onset,
    }
}

/// Calculate reliability only from events that participate in a transition
/// window. This prevents an unrelated noisy section elsewhere in a track
/// from dominating a local beat-match decision.
pub fn reliability_for_timeline_window(
    timeline: &BeatTimeline,
    start: Duration,
    end: Duration,
) -> ReliabilityBreakdown {
    if start > end {
        return ReliabilityBreakdown::unavailable();
    }
    let events = timeline.between(start, end).collect::<Vec<_>>();
    reliability_for_timeline(&BeatTimeline::new(events))
}

/// Median aggregation is deliberately used for confidence-like evidence.
/// A single bad frame or model spike must not dominate the reliability of an
/// otherwise stable transition window.
fn robust_median(values: &mut [f32]) -> Option<f32> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|left, right| left.total_cmp(right));
    let middle = values.len() / 2;
    if values.len().is_multiple_of(2) {
        Some((values[middle - 1] + values[middle]) * 0.5)
    } else {
        Some(values[middle])
    }
}

pub fn reliability_for_analysis(analysis: &TrackAnalysis) -> ReliabilityBreakdown {
    reliability_for_timeline(&timeline_from_analysis(analysis))
}

/// Track reliability used by V2's continuous target-confidence cost.
pub fn compute_reliability(analysis: &TrackAnalysis) -> f32 {
    reliability_for_analysis(analysis).reliability
}

/// Geometric pair reliability.  The geometric mean ensures one weak side is
/// not hidden by a strong side while preserving symmetry.
pub fn pair_reliability(outgoing: f32, incoming: f32) -> f32 {
    if !outgoing.is_finite() || !incoming.is_finite() {
        return 0.0;
    }
    (outgoing.clamp(0.0, 1.0) * incoming.clamp(0.0, 1.0))
        .sqrt()
        .clamp(0.0, 1.0)
}

pub fn compute_pair_reliability(outgoing: &TrackAnalysis, incoming: &TrackAnalysis) -> f32 {
    pair_reliability(compute_reliability(outgoing), compute_reliability(incoming))
}

pub fn compute_pair_reliability_for_timelines(
    outgoing: &BeatTimeline,
    incoming: &BeatTimeline,
) -> f32 {
    pair_reliability(
        reliability_for_timeline(outgoing).reliability,
        reliability_for_timeline(incoming).reliability,
    )
}

pub fn compute_reliability_v2(analysis: &TrackAnalysisV2) -> f32 {
    reliability_for_timeline(&timeline_from_track_analysis_v2(analysis)).reliability
}

pub fn reliability_for_track_analysis_v2(analysis: &TrackAnalysisV2) -> ReliabilityBreakdown {
    reliability_for_timeline(&timeline_from_track_analysis_v2(analysis))
}

pub fn reliability_for_rhythm(analysis: &RhythmAnalysis) -> ReliabilityBreakdown {
    reliability_for_timeline(&timeline_from_rhythm(analysis))
}

pub fn compute_pair_reliability_v2(outgoing: &TrackAnalysisV2, incoming: &TrackAnalysisV2) -> f32 {
    pair_reliability(
        compute_reliability_v2(outgoing),
        compute_reliability_v2(incoming),
    )
}

/// Compatibility alias for callers that prefer the shorter name.
pub fn reliability(analysis: &TrackAnalysis) -> f32 {
    compute_reliability(analysis)
}

/// Compatibility alias for callers that pass precomputed component values.
pub fn geometric_pair_reliability(outgoing: f32, incoming: f32) -> f32 {
    pair_reliability(outgoing, incoming)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn regular_timeline(confidence: Option<f32>) -> BeatTimeline {
        BeatTimeline::new(
            [0_u64, 500, 1_000, 1_500]
                .into_iter()
                .map(|millis| TimelineEvent {
                    time: Duration::from_millis(millis),
                    timing_confidence: Some(1.0),
                    beat_evidence: confidence,
                    generic_onset: None,
                })
                .collect(),
        )
    }

    #[test]
    fn reliability_keeps_timing_and_kick_evidence_separate() {
        let weak_timeline = regular_timeline(Some(0.20));
        let strong_timeline = regular_timeline(Some(0.90));
        let weak_kick = reliability_for_timeline(&weak_timeline);
        let strong_kick = reliability_for_timeline(&strong_timeline);

        // Timing remains perfect in both cases; changing only marker
        // confidence changes beat evidence, not event existence/continuity.
        assert_eq!(weak_kick.timing, 1.0);
        assert_eq!(strong_kick.timing, 1.0);
        assert_eq!(weak_kick.interval_continuity, 1.0);
        assert_eq!(strong_kick.interval_continuity, 1.0);
        assert!(strong_kick.beat_evidence > weak_kick.beat_evidence);
        assert!(strong_kick.reliability > weak_kick.reliability);
    }

    #[test]
    fn missing_marker_confidence_is_unknown_not_zero_evidence() {
        let missing_timeline = regular_timeline(None);
        let explicit_half_timeline = regular_timeline(Some(0.5));
        let missing = reliability_for_timeline(&missing_timeline);
        let explicit_half = reliability_for_timeline(&explicit_half_timeline);

        assert_eq!(missing.beat_evidence, 0.5);
        assert_eq!(missing.generic_onset, None);
        assert_eq!(missing.reliability, explicit_half.reliability);
    }

    #[test]
    fn reliability_rejects_non_monotonic_or_non_finite_confidence_without_panicking() {
        let malformed = BeatTimeline::new(vec![
            TimelineEvent {
                time: Duration::from_millis(500),
                timing_confidence: Some(f32::NAN),
                beat_evidence: None,
                generic_onset: None,
            },
            TimelineEvent {
                time: Duration::from_millis(500),
                timing_confidence: None,
                beat_evidence: None,
                generic_onset: None,
            },
        ]);
        let result = std::panic::catch_unwind(|| reliability_for_timeline(&malformed));
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), ReliabilityBreakdown::unavailable());
    }

    #[test]
    fn analysis_adapter_does_not_synthesize_events_from_bpm_or_first_beat() {
        let mut analysis = TrackAnalysis::unanalyzed(Duration::from_secs(30));
        analysis.bpm = Some(120.0);
        analysis.first_beat = Some(Duration::from_secs(1));

        let timeline = timeline_from_analysis(&analysis);
        assert!(timeline.is_empty());
        assert_eq!(compute_reliability(&analysis), 0.0);
    }

    #[test]
    fn pair_reliability_is_symmetric_and_does_not_hide_a_weak_side() {
        let weak_strong = pair_reliability(0.68, 1.0);
        let strong_weak = pair_reliability(1.0, 0.68);
        assert!((weak_strong - 0.68_f32.sqrt()).abs() < f32::EPSILON);
        assert_eq!(weak_strong, strong_weak);
        assert!(weak_strong < 1.0);
    }

    #[test]
    fn interval_continuity_and_unknown_onset_are_continuous() {
        let regular = regular_timeline(Some(1.0));
        let uneven = BeatTimeline::new(
            [0_u64, 400, 1_000, 1_500]
                .into_iter()
                .map(|millis| TimelineEvent {
                    time: Duration::from_millis(millis),
                    timing_confidence: Some(1.0),
                    beat_evidence: Some(1.0),
                    generic_onset: None,
                })
                .collect(),
        );
        let regular_reliability = reliability_for_timeline(&regular);
        let uneven_reliability = reliability_for_timeline(&uneven);
        assert_eq!(regular_reliability.generic_onset, None);
        assert_eq!(regular_reliability.interval_continuity, 1.0);
        assert!(uneven_reliability.interval_continuity < 1.0);
        assert!(uneven_reliability.reliability < regular_reliability.reliability);
    }

    #[test]
    fn transition_window_reliability_ignores_unrelated_track_events() {
        let mut events = regular_timeline(Some(1.0)).events;
        events.extend([
            TimelineEvent {
                time: Duration::from_secs(10),
                timing_confidence: Some(0.0),
                beat_evidence: Some(0.0),
                generic_onset: None,
            },
            TimelineEvent {
                time: Duration::from_secs(11),
                timing_confidence: Some(0.0),
                beat_evidence: Some(0.0),
                generic_onset: None,
            },
        ]);
        // The complete-track aggregate sees both regions, while a planner
        // window over the first phrase remains fully reliable.
        let timeline = BeatTimeline::new(events);
        assert!(reliability_for_timeline(&timeline).reliability < 1.0);
        assert_eq!(
            reliability_for_timeline_window(
                &timeline,
                Duration::ZERO,
                Duration::from_millis(1_500),
            )
            .reliability,
            reliability_for_timeline(&regular_timeline(Some(1.0))).reliability
        );
    }

    #[test]
    fn confidence_aggregates_ignore_a_single_transition_window_outlier() {
        let timeline = BeatTimeline::new(
            [
                (0_u64, 0.9, 0.8, 0.9),
                (500, 0.9, 0.8, 0.9),
                (1_000, 0.9, 0.8, 0.9),
                (1_500, 0.1, 0.0, 0.0),
            ]
            .into_iter()
            .map(|(millis, timing, evidence, onset)| TimelineEvent {
                time: Duration::from_millis(millis),
                timing_confidence: Some(timing),
                beat_evidence: Some(evidence),
                generic_onset: Some(onset),
            })
            .collect(),
        );
        let reliability = reliability_for_timeline(&timeline);

        assert_eq!(reliability.timing, 0.9);
        assert_eq!(reliability.beat_evidence, 0.8);
        assert_eq!(reliability.generic_onset, Some(0.9));
        assert!(reliability.reliability > 0.8);
    }
}
