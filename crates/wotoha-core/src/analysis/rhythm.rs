//! Beat, tempo, and meter observations for analysis schema V2.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::value::{Confidence, ModelScore, Support};

pub const MIN_TEMPO_BPM: f32 = 60.0;
pub const MAX_TEMPO_BPM: f32 = 220.0;
pub const METER_RESOLVE_MIN_SCORE: f32 = 0.60;
pub const METER_RESOLVE_MIN_MARGIN: f32 = 0.15;

/// A beat observation.  Tempo, bar, and local-BPM values are intentionally
/// not stored here: they are derived from the ordered event clock.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BeatEvent {
    pub time: Duration,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub beat_model_score: Option<ModelScore>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downbeat_model_score: Option<ModelScore>,
    pub timing_confidence: Confidence,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub onset_support: Option<Support>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub low_frequency_support: Option<Support>,
}

impl BeatEvent {
    pub fn new(
        time: Duration,
        beat_model_score: Option<ModelScore>,
        downbeat_model_score: Option<ModelScore>,
        timing_confidence: Confidence,
        onset_support: Option<Support>,
        low_frequency_support: Option<Support>,
    ) -> Self {
        Self {
            time,
            beat_model_score,
            downbeat_model_score,
            timing_confidence,
            onset_support,
            low_frequency_support,
        }
    }

    pub fn at(time: Duration, confidence: Confidence) -> Self {
        Self::new(time, None, None, confidence, None, None)
    }

    pub fn validate(&self) -> bool {
        self.timing_confidence.validate()
            && self
                .beat_model_score
                .as_ref()
                .is_none_or(ModelScore::validate)
            && self
                .downbeat_model_score
                .as_ref()
                .is_none_or(ModelScore::validate)
            && self.onset_support.as_ref().is_none_or(Support::validate)
            && self
                .low_frequency_support
                .as_ref()
                .is_none_or(Support::validate)
    }

    pub fn score(&self) -> Option<ModelScore> {
        self.beat_model_score
    }

    pub fn downbeat(&self) -> Option<ModelScore> {
        self.downbeat_model_score
    }

    pub fn confidence(&self) -> Confidence {
        self.timing_confidence
    }
}

/// How a tempo candidate relates to the primary beat interpretation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TempoRelation {
    #[default]
    Primary,
    HalfTime,
    DoubleTime,
    Alternative,
}

pub type Relation = TempoRelation;

/// A tempo candidate. BPM is kept only at hypothesis level, never per beat.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct TempoHypothesis {
    pub bpm: f32,
    pub relative_weight: super::value::UnitInterval,
    #[serde(default)]
    pub relation: TempoRelation,
}

impl TempoHypothesis {
    pub fn new(
        bpm: f32,
        relative_weight: super::value::UnitInterval,
        relation: TempoRelation,
    ) -> Option<Self> {
        Self::with_relation(bpm, relative_weight, relation)
    }

    pub fn with_relation(
        bpm: f32,
        relative_weight: super::value::UnitInterval,
        relation: TempoRelation,
    ) -> Option<Self> {
        (bpm.is_finite() && bpm > 0.0).then_some(Self {
            bpm,
            relative_weight,
            relation,
        })
    }

    pub fn validate(&self) -> bool {
        self.bpm.is_finite() && self.bpm > 0.0 && self.relative_weight.validate()
    }
}

impl Default for TempoHypothesis {
    fn default() -> Self {
        Self {
            bpm: 120.0,
            relative_weight: super::value::UnitInterval::ZERO,
            relation: TempoRelation::Primary,
        }
    }
}

/// A meter candidate represented by beats per bar and its downbeat phase.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct MeterHypothesis {
    pub beats_per_bar: u8,
    pub downbeat_phase: u8,
    pub score: super::value::UnitInterval,
}

impl MeterHypothesis {
    pub fn new(
        beats_per_bar: u8,
        downbeat_phase: u8,
        score: super::value::UnitInterval,
    ) -> Option<Self> {
        (matches!(beats_per_bar, 2 | 3 | 4 | 6) && downbeat_phase < beats_per_bar).then_some(Self {
            beats_per_bar,
            downbeat_phase,
            score,
        })
    }

    pub fn validate(&self) -> bool {
        matches!(self.beats_per_bar, 2 | 3 | 4 | 6)
            && self.downbeat_phase < self.beats_per_bar
            && self.score.validate()
    }

    pub fn meter(&self) -> u8 {
        self.beats_per_bar
    }
}

impl Default for MeterHypothesis {
    fn default() -> Self {
        Self {
            beats_per_bar: 4,
            downbeat_phase: 0,
            score: super::value::UnitInterval::ZERO,
        }
    }
}

/// Ordered rhythm observations and competing interpretations.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RhythmAnalysis {
    pub beats: Vec<BeatEvent>,
    #[serde(default)]
    pub tempo_hypotheses: Vec<TempoHypothesis>,
    #[serde(default)]
    pub meter_hypotheses: Vec<MeterHypothesis>,
}

impl RhythmAnalysis {
    pub fn new(
        beats: Vec<BeatEvent>,
        tempo_hypotheses: Vec<TempoHypothesis>,
        meter_hypotheses: Vec<MeterHypothesis>,
    ) -> Option<Self> {
        let analysis = Self {
            beats,
            tempo_hypotheses,
            meter_hypotheses,
        };
        analysis.validate().then_some(analysis)
    }

    /// Builds rhythm analysis from a beat clock and robustly derives its
    /// primary tempo.  Interval outliers are rejected using median/MAD.
    pub fn from_beats(beats: Vec<BeatEvent>) -> Option<Self> {
        let mut analysis = Self::new(beats, Vec::new(), Vec::new())?;
        if let Some(bpm) = robust_tempo(&analysis.beats) {
            analysis.tempo_hypotheses.push(TempoHypothesis {
                bpm,
                relative_weight: super::value::UnitInterval::ONE,
                relation: TempoRelation::Primary,
            });
        }
        Some(analysis)
    }

    pub fn validate(&self) -> bool {
        self.beats.iter().all(BeatEvent::validate)
            && self
                .beats
                .windows(2)
                .all(|window| window[0].time < window[1].time)
            && self.tempo_hypotheses.iter().all(TempoHypothesis::validate)
            && self.meter_hypotheses.iter().all(MeterHypothesis::validate)
    }

    /// Returns the strongest tempo candidate, falling back to robust beat
    /// interval estimation when no candidate was persisted.
    pub fn primary_tempo(&self) -> Option<f32> {
        self.primary_tempo_hypothesis()
            .map(|hypothesis| hypothesis.bpm)
            .or_else(|| robust_tempo(&self.beats))
    }

    pub fn primary_tempo_hypothesis(&self) -> Option<&TempoHypothesis> {
        self.tempo_hypotheses
            .iter()
            .filter(|h| h.validate())
            .max_by(|a, b| {
                let a_score = a.relative_weight.get();
                let b_score = b.relative_weight.get();
                a_score.total_cmp(&b_score)
            })
    }

    /// Estimates tempo around a clock position from nearby intervals.
    pub fn local_tempo_at(&self, position: Duration) -> Option<f32> {
        let nearest = self.nearest_beat_index(position)?;
        self.local_tempo_at_index(nearest)
    }

    /// Estimates tempo around an existing beat index from nearby intervals.
    ///
    /// The index form is useful to consumers that already operate on the
    /// ordered beat clock and avoids an unnecessary time-to-index lookup.
    pub fn local_tempo_at_index(&self, index: usize) -> Option<f32> {
        self.beats.get(index)?;
        let from = index.saturating_sub(4);
        let to = index.saturating_add(5).min(self.beats.len());
        robust_tempo_for_range(&self.beats[from..to]).or_else(|| self.primary_tempo())
    }

    pub fn nearest_beat(&self, position: Duration) -> Option<&BeatEvent> {
        self.nearest_beat_index(position)
            .and_then(|index| self.beats.get(index))
    }

    pub fn nearest_beat_index(&self, position: Duration) -> Option<usize> {
        if self.beats.is_empty() {
            return None;
        }
        match self.beats.binary_search_by_key(&position, |beat| beat.time) {
            Ok(index) => Some(index),
            Err(0) => Some(0),
            Err(index) if index >= self.beats.len() => Some(self.beats.len() - 1),
            Err(index) => {
                let before = self.beats[index - 1].time.abs_diff(position);
                let after = self.beats[index].time.abs_diff(position);
                Some(if before <= after { index - 1 } else { index })
            }
        }
    }

    /// Returns events in the inclusive clock interval. Reversed bounds are
    /// rejected rather than silently changing the caller's intent.
    pub fn beats_between(&self, start: Duration, end: Duration) -> Vec<&BeatEvent> {
        if start > end {
            return Vec::new();
        }
        self.beats
            .iter()
            .filter(|beat| (start..=end).contains(&beat.time))
            .collect()
    }

    /// Resolves meter only when the best candidate is both confident and
    /// meaningfully separated from its runner-up.
    pub fn resolved_meter(&self) -> Option<u8> {
        self.resolved_meter_hypothesis().map(MeterHypothesis::meter)
    }

    pub fn resolved_meter_hypothesis(&self) -> Option<&MeterHypothesis> {
        let mut ranked = self
            .meter_hypotheses
            .iter()
            .filter(|hypothesis| hypothesis.validate())
            .collect::<Vec<_>>();
        ranked.sort_by(|a, b| b.score.get().total_cmp(&a.score.get()));
        let best = ranked.first().copied()?;
        let second = ranked.get(1).map_or(0.0, |candidate| candidate.score.get());
        (best.score.get() >= METER_RESOLVE_MIN_SCORE
            && best.score.get() - second >= METER_RESOLVE_MIN_MARGIN)
            .then_some(best)
    }

    /// Returns the zero-based beat position within the resolved bar.
    pub fn bar_position(&self, position: Duration) -> Option<u8> {
        let index = self.nearest_beat_index(position)?;
        self.bar_position_at_index(index)
    }

    /// Returns the zero-based beat position within the resolved bar for an
    /// existing beat index.
    pub fn bar_position_at_index(&self, index: usize) -> Option<u8> {
        self.beats.get(index)?;
        let numerator = self.resolved_meter()?;
        let downbeat_phase = self
            .resolved_meter_hypothesis()
            .map_or(0, |meter| meter.downbeat_phase as usize);
        Some(((index + numerator as usize - downbeat_phase) % numerator as usize) as u8)
    }

    pub fn downbeat_phase(&self) -> Option<usize> {
        self.beats
            .iter()
            .enumerate()
            .filter_map(|(index, beat)| beat.downbeat_model_score.map(|score| (index, score.get())))
            .max_by(|left, right| left.1.total_cmp(&right.1))
            .map(|(index, _)| index)
    }
}

fn robust_tempo(beats: &[BeatEvent]) -> Option<f32> {
    robust_tempo_for_range(beats)
}

fn robust_tempo_for_range(beats: &[BeatEvent]) -> Option<f32> {
    if beats.len() < 2 {
        return None;
    }
    let intervals = beats
        .windows(2)
        .map(|window| window[1].time.as_secs_f64() - window[0].time.as_secs_f64())
        .filter(|interval| interval.is_finite() && *interval > 0.0)
        .collect::<Vec<_>>();
    if intervals.is_empty() {
        return None;
    }
    let interval_median = median(&intervals)?;
    let deviations = intervals
        .iter()
        .map(|interval| (interval - interval_median).abs())
        .collect::<Vec<_>>();
    let mad = median(&deviations)?;
    let tolerance = if mad <= f64::EPSILON {
        interval_median * 0.08
    } else {
        (3.0 * mad).max(interval_median * 0.02)
    };
    let inliers = intervals
        .iter()
        .copied()
        .filter(|interval| (interval - interval_median).abs() <= tolerance)
        .collect::<Vec<_>>();
    if inliers.len() * 2 < intervals.len() {
        return None;
    }
    let interval = median(&inliers)?;
    let bpm: f64 = 60.0 / interval;
    (bpm.is_finite() && (MIN_TEMPO_BPM..=MAX_TEMPO_BPM).contains(&(bpm as f32)))
        .then_some(bpm as f32)
}

fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let middle = sorted.len() / 2;
    Some(if sorted.len().is_multiple_of(2) {
        (sorted[middle - 1] + sorted[middle]) / 2.0
    } else {
        sorted[middle]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn beat(time: u64) -> BeatEvent {
        BeatEvent::at(Duration::from_millis(time), Confidence::ONE)
    }

    #[test]
    fn rejects_non_monotonic_beat_clock_and_stores_no_bpm_on_events() {
        let beats = vec![beat(0), beat(500), beat(500)];
        assert!(RhythmAnalysis::new(beats, Vec::new(), Vec::new()).is_none());
        assert_eq!(beat(0).beat_model_score, None);
    }

    #[test]
    fn robust_tempo_ignores_one_interval_outlier() {
        let beats = [0, 500, 1_000, 1_500, 2_000, 3_250]
            .into_iter()
            .map(beat)
            .collect();
        let analysis = RhythmAnalysis::from_beats(beats).unwrap();
        assert!((analysis.primary_tempo().unwrap() - 120.0).abs() < 0.1);
        assert!((analysis.local_tempo_at_index(2).unwrap() - 120.0).abs() < 0.1);
        assert!(analysis.local_tempo_at_index(99).is_none());
    }

    #[test]
    fn meter_requires_confidence_and_margin() {
        let weak =
            MeterHypothesis::new(4, 0, super::super::value::UnitInterval::clamped(0.7)).unwrap();
        let ambiguous =
            MeterHypothesis::new(3, 0, super::super::value::UnitInterval::clamped(0.64)).unwrap();
        let analysis = RhythmAnalysis::new(Vec::new(), Vec::new(), vec![weak, ambiguous]).unwrap();
        assert!(analysis.resolved_meter().is_none());
        let clear =
            MeterHypothesis::new(3, 0, super::super::value::UnitInterval::clamped(0.8)).unwrap();
        let analysis = RhythmAnalysis::new(
            Vec::new(),
            Vec::new(),
            vec![
                clear,
                MeterHypothesis::new(4, 0, super::super::value::UnitInterval::clamped(0.5))
                    .unwrap(),
            ],
        )
        .unwrap();
        assert_eq!(analysis.resolved_meter(), Some(3));
    }

    #[test]
    fn kick_score_support_and_timing_confidence_are_independent() {
        // A weak low-band/kick score must not erase a strongly timed BeatThis
        // event.  The three fields are deliberately separate evidence paths.
        let event = BeatEvent::new(
            Duration::from_millis(500),
            Some(ModelScore::new(0.15).unwrap()),
            None,
            Confidence::new(0.92).unwrap(),
            Some(Support::new(0.20).unwrap()),
            Some(Support::new(0.25).unwrap()),
        );

        assert!(event.validate());
        assert_eq!(event.beat_model_score.unwrap().get(), 0.15);
        assert_eq!(event.onset_support.unwrap().get(), 0.20);
        assert_eq!(event.timing_confidence.get(), 0.92);
    }

    #[test]
    fn tempo_domain_accepts_positive_finite_values() {
        for bpm in [
            0.001, 59.99, 60.0, 85.0, 120.0, 170.0, 220.0, 220.01, 10_000.0,
        ] {
            assert!(
                TempoHypothesis::new(
                    bpm,
                    super::super::value::UnitInterval::ONE,
                    TempoRelation::Primary,
                )
                .is_some(),
                "{bpm} BPM should be a valid positive finite domain value"
            );
        }
        for bpm in [0.0, -0.001, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert!(
                TempoHypothesis::new(
                    bpm,
                    super::super::value::UnitInterval::ONE,
                    TempoRelation::Primary,
                )
                .is_none(),
                "{bpm} BPM should be rejected"
            );
        }
    }

    #[test]
    fn robust_tempo_tolerates_small_clock_drift_without_regenerating_beats() {
        let beats = [750, 1_250, 1_752, 2_254, 2_756, 3_258]
            .into_iter()
            .map(beat)
            .collect::<Vec<_>>();
        let analysis = RhythmAnalysis::from_beats(beats.clone()).unwrap();

        assert!((analysis.primary_tempo().unwrap() - 119.5).abs() < 1.0);
        assert_eq!(analysis.beats, beats);
    }

    #[test]
    fn missing_and_pickup_events_are_explicit_and_safe() {
        let missing = RhythmAnalysis::from_beats(Vec::new()).unwrap();
        assert_eq!(missing.primary_tempo(), None);
        assert!(
            missing
                .beats_between(Duration::from_secs(2), Duration::from_secs(1))
                .is_empty()
        );

        // A pickup is a real first event at a non-zero source time.  It must
        // remain the first event instead of being normalized to t=0.
        let pickup =
            RhythmAnalysis::from_beats([750, 1_250, 1_750, 2_250].into_iter().map(beat).collect())
                .unwrap();
        assert_eq!(
            pickup.beats.first().map(|event| event.time),
            Some(Duration::from_millis(750))
        );
        assert!((pickup.primary_tempo().unwrap() - 120.0).abs() < 0.1);
    }

    #[test]
    fn malformed_rhythm_is_rejected_without_panicking() {
        let malformed_tempo = TempoHypothesis {
            bpm: f32::NAN,
            relative_weight: super::super::value::UnitInterval::ONE,
            relation: TempoRelation::Primary,
        };
        let result = std::panic::catch_unwind(|| {
            RhythmAnalysis::new(
                vec![beat(1_000), beat(500)],
                vec![malformed_tempo],
                Vec::new(),
            )
        });

        assert!(
            result.is_ok(),
            "malformed rhythm must fail closed, not panic"
        );
        assert!(result.unwrap().is_none());
        assert!(
            TempoHypothesis::new(
                f32::INFINITY,
                super::super::value::UnitInterval::ONE,
                TempoRelation::Primary,
            )
            .is_none()
        );
    }

    #[test]
    fn resolved_meter_supports_4_4_and_3_4_but_rejects_ambiguous_candidates() {
        let beats = (0..8).map(|index| beat(index * 500)).collect::<Vec<_>>();
        let four_four = RhythmAnalysis::new(
            beats.clone(),
            Vec::new(),
            vec![
                MeterHypothesis::new(4, 0, super::super::value::UnitInterval::clamped(0.85))
                    .unwrap(),
            ],
        )
        .unwrap();
        assert_eq!(four_four.resolved_meter(), Some(4));
        assert_eq!(
            four_four.bar_position(Duration::from_millis(2_000)),
            Some(0)
        );
        assert_eq!(four_four.bar_position_at_index(4), Some(0));
        assert_eq!(four_four.bar_position_at_index(99), None);

        let three_four = RhythmAnalysis::new(
            beats.clone(),
            Vec::new(),
            vec![
                MeterHypothesis::new(3, 0, super::super::value::UnitInterval::clamped(0.85))
                    .unwrap(),
            ],
        )
        .unwrap();
        assert_eq!(three_four.resolved_meter(), Some(3));

        let ambiguous = RhythmAnalysis::new(
            beats,
            Vec::new(),
            vec![
                MeterHypothesis::new(4, 0, super::super::value::UnitInterval::clamped(0.71))
                    .unwrap(),
                MeterHypothesis::new(3, 0, super::super::value::UnitInterval::clamped(0.70))
                    .unwrap(),
            ],
        )
        .unwrap();
        assert_eq!(ambiguous.resolved_meter(), None);
    }
}
