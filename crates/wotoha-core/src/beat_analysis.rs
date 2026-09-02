//! Neural beat observations and deterministic beat-grid post-processing.
//!
//! This module deliberately does not depend on an inference runtime.  The runtime
//! crate feeds it the activation streams produced by Beat This!, while
//! the classical analyser remains responsible for all other track features.  Keeping
//! the decoder here makes the bounded DP and the onset refinement easy to test with
//! generated observations, without loading a 10 MiB ONNX model in core tests.

use std::time::Duration;

use crate::{
    analysis::{
        BeatEvent, Confidence, MeterHypothesis, ModelScore, RhythmAnalysis, Support,
        TempoHypothesis as DomainTempoHypothesis, TempoRelation as DomainTempoRelation,
        UnitInterval,
    },
    automix::TrackAnalysis,
};

/// Bounds used by the neural decoder. These are intentionally narrower than
/// the wider tempo range accepted by playback.
pub const MIN_BPM: f32 = 60.0;
pub const MAX_BPM: f32 = 220.0;
const MIN_PATH_MARKERS: usize = 4;
const START_PENALTY: f32 = 0.60;
const MIN_COVERAGE: f32 = 0.45;
const MIN_DOWNBEAT_MARGIN: f32 = 0.06;
const MIN_NORMALIZED_DOWNBEAT_MARGIN: f32 = 0.18;
const FRAME_RATE: f32 = 50.0;
const REFINE_WINDOW: Duration = Duration::from_millis(40);
const MIN_REFINED_SPACING: Duration = Duration::from_millis(170);

/// Raw neural beat and downbeat activations.
///
/// Values are logits (not probabilities), as returned by `beat-this`.  The
/// decoder validation rejects non-finite values before using them, so malformed
/// model output cannot poison a cached `TrackAnalysis`.
#[derive(Clone, Debug, PartialEq)]
pub struct NeuralBeatObservations {
    pub frame_rate_hz: f32,
    pub beat_logits: Vec<f32>,
    pub downbeat_logits: Vec<f32>,
}

/// A beat event decoded from the neural path.
///
/// `time` is the event's source-of-truth timestamp. Callers must not discard
/// these events and regenerate a uniform grid from a tempo summary. The
/// support values intentionally remain separate: timing confidence describes
/// the bounded-DP path, onset support describes the neural activation, and
/// low-frequency support describes kick-band evidence.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NeuralBeatEvent {
    pub time: Duration,
    /// Raw model-derived score for this event (sigmoid beat logit).
    pub model_score: f32,
    /// Raw downbeat score at this event (sigmoid downbeat logit).
    pub downbeat_score: f32,
    pub timing_confidence: f32,
    pub onset_support: f32,
    pub low_frequency_support: Option<f32>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TempoRelation {
    #[default]
    Primary,
    HalfTime,
    DoubleTime,
    Alternative,
}

/// One valid member of the explicit half/native/double-time tempo family.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TempoHypothesis {
    pub bpm: f32,
    pub relative_weight: f32,
    pub relation: TempoRelation,
}

/// Decoder-owned rhythm result, independent of the legacy `TrackAnalysis`
/// struct. The analysis domain can map this value to its own `RhythmAnalysis`
/// without coupling the decoder to a mutable aggregate.
#[derive(Clone, Debug, PartialEq)]
pub struct NeuralRhythmAnalysis {
    pub beat_events: Vec<NeuralBeatEvent>,
    pub primary_bpm: f32,
    pub tempo_hypotheses: Vec<TempoHypothesis>,
    pub beat_confidence: f32,
    pub first_downbeat_index: Option<usize>,
    pub downbeat_confidence: f32,
}

impl NeuralRhythmAnalysis {
    /// The selected path's tempo. Beat-event timestamps remain authoritative.
    pub const fn primary_tempo(&self) -> f32 {
        self.primary_bpm
    }

    /// Ordered event clock, exposed as a slice to discourage grid synthesis.
    pub fn events(&self) -> &[NeuralBeatEvent] {
        &self.beat_events
    }

    /// Convert the decoder result into the versioned domain record without
    /// rebuilding a beat grid from its tempo summary. Event timestamps and all
    /// per-event evidence are copied one-for-one.
    pub fn into_rhythm_analysis(self) -> Option<RhythmAnalysis> {
        let beats = self
            .beat_events
            .into_iter()
            .map(|event| {
                Some(BeatEvent::new(
                    event.time,
                    ModelScore::new(event.model_score),
                    ModelScore::new(event.downbeat_score),
                    Confidence::new(event.timing_confidence)?,
                    Support::new(event.onset_support),
                    event.low_frequency_support.and_then(Support::new),
                ))
            })
            .collect::<Option<Vec<_>>>()?;
        let tempo_hypotheses = self
            .tempo_hypotheses
            .into_iter()
            .filter_map(|hypothesis| {
                let relation = match hypothesis.relation {
                    TempoRelation::Primary => DomainTempoRelation::Primary,
                    TempoRelation::HalfTime => DomainTempoRelation::HalfTime,
                    TempoRelation::DoubleTime => DomainTempoRelation::DoubleTime,
                    TempoRelation::Alternative => DomainTempoRelation::Alternative,
                };
                DomainTempoHypothesis::new(
                    hypothesis.bpm,
                    UnitInterval::new(hypothesis.relative_weight)?,
                    relation,
                )
            })
            .collect::<Vec<_>>();
        let meter_hypotheses = meter_hypotheses_from_events(
            &beats,
            self.first_downbeat_index,
            self.downbeat_confidence,
        );
        RhythmAnalysis::new(beats, tempo_hypotheses, meter_hypotheses)
    }
}

impl NeuralBeatObservations {
    pub fn new(beat_logits: Vec<f32>, downbeat_logits: Vec<f32>) -> Option<Self> {
        Self::with_frame_rate(FRAME_RATE, beat_logits, downbeat_logits)
    }

    pub fn with_frame_rate(
        frame_rate_hz: f32,
        beat_logits: Vec<f32>,
        downbeat_logits: Vec<f32>,
    ) -> Option<Self> {
        (frame_rate_hz.is_finite()
            && frame_rate_hz > 0.0
            && beat_logits.len() == downbeat_logits.len()
            && !beat_logits.is_empty())
        .then_some(Self {
            frame_rate_hz,
            beat_logits,
            downbeat_logits,
        })
    }

    pub fn len(&self) -> usize {
        self.beat_logits.len()
    }

    pub fn is_empty(&self) -> bool {
        self.beat_logits.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq)]
struct DecodedGrid {
    frames: Vec<usize>,
    marker_confidences: Vec<f32>,
    period_frames: usize,
    bpm: f32,
    beat_confidence: f32,
    first_downbeat_ordinal: Option<usize>,
    downbeat_confidence: f32,
}

/// Replace only beat-related fields in an existing classical analysis.
///
/// Returns `true` when the neural grid was sufficiently supported.  A `false`
/// result leaves `analysis` untouched, which is the explicit fallback used for
/// silence, non-periodic material, malformed output, and unsupported tracks.
pub fn apply_neural_beat_observations(
    analysis: &mut TrackAnalysis,
    observations: &NeuralBeatObservations,
    low_band_1khz: &[f32],
) -> bool {
    let Some(grid) = decode_grid(observations) else {
        return false;
    };
    if grid.frames.len() < MIN_PATH_MARKERS {
        return false;
    }

    let low_onset = low_band_onset(low_band_1khz);
    let (refined_samples, refined_support) =
        refine_markers(&grid.frames, &low_onset, observations.frame_rate_hz);
    let mut markers = Vec::with_capacity(grid.frames.len());
    let mut confidences = Vec::with_capacity(grid.frames.len());
    let min_sample = (analysis.audible_start.as_secs_f32() * 1_000.0)
        .floor()
        .max(0.0) as usize;
    let max_sample = (analysis.audible_end.as_secs_f32() * 1_000.0)
        .ceil()
        .max(0.0) as usize;
    let visible_downbeat_ordinal = grid.first_downbeat_ordinal.and_then(|ordinal| {
        let phase = ordinal % 4;
        let mut visible_index = 0_usize;
        for (original_index, sample) in refined_samples.iter().copied().enumerate() {
            let visible = (min_sample..=max_sample).contains(&sample);
            if visible && original_index >= ordinal && original_index % 4 == phase {
                return Some(visible_index);
            }
            if visible {
                visible_index += 1;
            }
        }
        None
    });
    for (marker_index, (_frame, confidence)) in grid
        .frames
        .iter()
        .copied()
        .zip(grid.marker_confidences.iter().copied())
        .enumerate()
    {
        let Some(sample) = refined_samples.get(marker_index).copied() else {
            return false;
        };
        if sample < min_sample || sample > max_sample {
            continue;
        }
        let Some(marker) = millis_duration(sample) else {
            return false;
        };
        markers.push(marker);
        let model_activation = observations
            .beat_logits
            .get(grid.frames[marker_index])
            .copied()
            .map(logit_probability)
            .unwrap_or_default();
        let confidence = if model_activation < 0.5
            || refined_support
                .get(marker_index)
                .copied()
                .unwrap_or_default()
                < 0.35
        {
            confidence.min(0.20)
        } else {
            confidence
        };
        confidences.push(confidence.clamp(0.0, 1.0));
    }
    if markers.len() < MIN_PATH_MARKERS {
        return false;
    }

    let first_downbeat = visible_downbeat_ordinal.and_then(|ordinal| markers.get(ordinal).copied());
    // The model grid chooses an integer 20 ms frame, but the public marker
    // clock is refined against the 1 kHz onset stream. Derive the final tempo
    // from those refined intervals so marker precision is not thrown away.
    let Some(bpm) = refined_bpm(&markers) else {
        return false;
    };
    analysis.bpm = Some(bpm);
    analysis.beat_confidence = grid.beat_confidence;
    analysis.first_beat = markers.first().copied();
    analysis.beat_markers = markers;
    analysis.beat_marker_confidences = confidences;
    analysis.first_downbeat = if grid.downbeat_confidence > 0.0 {
        first_downbeat
    } else {
        None
    };
    analysis.downbeat_confidence = grid.downbeat_confidence;
    true
}

/// Decode neural observations into an event-preserving rhythm result.
///
/// The bounded-DP path supplies event positions and the primary tempo. The
/// primary tempo is never used to synthesize additional events. For callers
/// that have the low-frequency stream, use
/// [`decode_neural_rhythm_with_low_frequency`] so ±40 ms refinement can be
/// applied while retaining the model score and event existence.
pub fn decode_neural_rhythm(observations: &NeuralBeatObservations) -> Option<NeuralRhythmAnalysis> {
    let grid = decode_grid(observations)?;
    rhythm_from_grid(observations, &grid, None, None)
}

/// Core-facing V2 convenience API. The decoder-owned intermediate remains
/// available from [`decode_neural_rhythm`], while this spelling returns the
/// stable `RhythmAnalysis` domain value used by the planner.
pub fn decode_neural_rhythm_analysis(
    observations: &NeuralBeatObservations,
) -> Option<RhythmAnalysis> {
    decode_neural_rhythm(observations)?.into_rhythm_analysis()
}

/// Decode neural observations and refine each event against a low-frequency
/// onset stream. The low-frequency stream only contributes timing/support
/// metadata; it cannot delete a model event or overwrite its neural score.
pub fn decode_neural_rhythm_with_low_frequency(
    observations: &NeuralBeatObservations,
    low_band_1khz: &[f32],
) -> Option<NeuralRhythmAnalysis> {
    let grid = decode_grid(observations)?;
    let low_onset = low_band_onset(low_band_1khz);
    let (refined_samples, refined_support) =
        refine_markers(&grid.frames, &low_onset, observations.frame_rate_hz);
    rhythm_from_grid(
        observations,
        &grid,
        Some(&refined_samples),
        Some(&refined_support),
    )
}

/// Low-frequency-refined counterpart of [`decode_neural_rhythm_analysis`].
pub fn decode_neural_rhythm_analysis_with_low_frequency(
    observations: &NeuralBeatObservations,
    low_band_1khz: &[f32],
) -> Option<RhythmAnalysis> {
    decode_neural_rhythm_with_low_frequency(observations, low_band_1khz)?.into_rhythm_analysis()
}

fn meter_hypotheses_from_events(
    beats: &[BeatEvent],
    first_downbeat_index: Option<usize>,
    downbeat_confidence: f32,
) -> Vec<MeterHypothesis> {
    if beats.is_empty() {
        return Vec::new();
    }
    let mut hypotheses = Vec::with_capacity(4);
    for beats_per_bar in [2_u8, 3, 4, 6] {
        let mut best = None;
        for phase in 0..beats_per_bar {
            let mut scores = beats
                .iter()
                .enumerate()
                .filter(|(index, _)| *index % usize::from(beats_per_bar) == usize::from(phase))
                .filter_map(|(_, event)| event.downbeat_model_score.map(|score| score.get()))
                .collect::<Vec<_>>();
            if scores.is_empty() {
                continue;
            }
            scores.sort_by(f32::total_cmp);
            let from = scores.len() / 2;
            let mean = scores[from..].iter().sum::<f32>() / (scores.len() - from).max(1) as f32;
            let Some(score) = UnitInterval::new(mean) else {
                continue;
            };
            let Some(candidate) = MeterHypothesis::new(beats_per_bar, phase, score) else {
                continue;
            };
            if best
                .as_ref()
                .is_none_or(|current: &MeterHypothesis| candidate.score.get() > current.score.get())
            {
                best = Some(candidate);
            }
        }
        if let Some(candidate) = best {
            hypotheses.push(candidate);
        }
    }
    if let (Some(index), true) = (first_downbeat_index, downbeat_confidence.is_finite())
        && downbeat_confidence > 0.0
        && let Some(existing) = hypotheses
            .iter_mut()
            .find(|hypothesis| hypothesis.beats_per_bar == 4)
    {
        existing.downbeat_phase = (index % 4) as u8;
        existing.score = UnitInterval::clamped(existing.score.get().max(downbeat_confidence));
    }
    hypotheses.sort_by(|left, right| right.score.get().total_cmp(&left.score.get()));
    hypotheses
}

fn rhythm_from_grid(
    observations: &NeuralBeatObservations,
    grid: &DecodedGrid,
    refined_samples: Option<&[usize]>,
    low_frequency_support: Option<&[f32]>,
) -> Option<NeuralRhythmAnalysis> {
    if grid.frames.len() < MIN_PATH_MARKERS
        || refined_samples.is_some_and(|samples| samples.len() != grid.frames.len())
        || low_frequency_support.is_some_and(|support| support.len() != grid.frames.len())
    {
        return None;
    }
    let beat_events = grid
        .frames
        .iter()
        .enumerate()
        .map(|(index, frame)| {
            let model_score = observations
                .beat_logits
                .get(*frame)
                .copied()
                .map(logit_probability)
                .unwrap_or_default();
            let downbeat_score = observations
                .downbeat_logits
                .get(*frame)
                .copied()
                .map(logit_probability)
                .unwrap_or_default();
            let time = refined_samples
                .and_then(|samples| samples.get(index).copied())
                .and_then(millis_duration)
                .or_else(|| frame_duration(*frame, observations.frame_rate_hz));
            let time = time?;
            Some(NeuralBeatEvent {
                time,
                model_score,
                downbeat_score,
                timing_confidence: grid.marker_confidences[index].clamp(0.0, 1.0),
                onset_support: model_score,
                low_frequency_support: low_frequency_support
                    .and_then(|support| support.get(index).copied())
                    .map(|support| support.clamp(0.0, 1.0)),
            })
        })
        .collect::<Option<Vec<_>>>()?;
    if beat_events.len() < MIN_PATH_MARKERS
        || beat_events
            .windows(2)
            .any(|window| window[0].time >= window[1].time)
        || beat_events.iter().any(|event| {
            !event.time.as_secs_f32().is_finite()
                || !event.model_score.is_finite()
                || !event.downbeat_score.is_finite()
                || !event.timing_confidence.is_finite()
                || !event.onset_support.is_finite()
                || event
                    .low_frequency_support
                    .is_some_and(|support| !support.is_finite())
        })
    {
        return None;
    }
    let tempo_hypotheses = tempo_hypotheses(observations, &grid.frames, grid.period_frames);
    Some(NeuralRhythmAnalysis {
        beat_events,
        primary_bpm: grid.bpm,
        tempo_hypotheses,
        beat_confidence: grid.beat_confidence,
        first_downbeat_index: grid.first_downbeat_ordinal,
        downbeat_confidence: grid.downbeat_confidence,
    })
}

/// Decode a pair of activation streams into a bounded, monotonic beat grid.
///
/// The dynamic program is Ellis-style: each predecessor is constrained to the
/// 60--220 BPM interval and receives a local-period continuity penalty.  Low
/// activation frames remain eligible, allowing one missing observation to be
/// represented on the grid rather than causing an offbeat jump.
fn frame_duration(frame: usize, frame_rate_hz: f32) -> Option<Duration> {
    if !frame_rate_hz.is_finite() || frame_rate_hz <= 0.0 {
        return None;
    }
    let seconds = frame as f64 / frame_rate_hz as f64;
    (seconds.is_finite() && seconds >= 0.0 && seconds <= Duration::MAX.as_secs_f64())
        .then(|| Duration::from_secs_f64(seconds))
}

fn millis_duration(milliseconds: usize) -> Option<Duration> {
    let seconds = u64::try_from(milliseconds / 1_000).ok()?;
    let remainder = milliseconds % 1_000;
    Some(Duration::new(seconds, remainder as u32 * 1_000_000))
}

pub fn decode_neural_grid(observations: &NeuralBeatObservations) -> Option<Vec<Duration>> {
    let grid = decode_grid(observations)?;
    grid.frames
        .into_iter()
        .map(|frame| frame_duration(frame, observations.frame_rate_hz))
        .collect()
}

fn decode_grid(observations: &NeuralBeatObservations) -> Option<DecodedGrid> {
    if observations.len() < MIN_PATH_MARKERS
        || !observations.frame_rate_hz.is_finite()
        || observations.frame_rate_hz <= 0.0
        || observations
            .beat_logits
            .iter()
            .chain(observations.downbeat_logits.iter())
            .any(|value| !value.is_finite())
    {
        return None;
    }
    let beat: Vec<f32> = observations
        .beat_logits
        .iter()
        .copied()
        .map(logit_probability)
        .collect();
    let downbeat: Vec<f32> = observations
        .downbeat_logits
        .iter()
        .copied()
        .map(logit_probability)
        .collect();
    let frames = bounded_dp(&beat, observations.frame_rate_hz)?;
    let intervals = frames
        .windows(2)
        .map(|pair| pair[1] - pair[0])
        .collect::<Vec<_>>();
    let period = robust_period(&intervals, observations.frame_rate_hz)?;
    let alias_margin = alias_hypothesis_margin(&beat, &frames, period, observations.frame_rate_hz);
    let bpm = 60.0 * observations.frame_rate_hz / period as f32;
    if !(MIN_BPM..=MAX_BPM).contains(&bpm) {
        return None;
    }
    let coverage = frames.iter().filter(|frame| beat[**frame] >= 0.5).count() as f32
        / frames.len().max(1) as f32;
    let residual = intervals
        .iter()
        .map(|interval| (*interval as f32 - period as f32).abs() / period as f32)
        .sum::<f32>()
        / intervals.len().max(1) as f32;
    let support = beat
        .iter()
        .enumerate()
        .filter(|(_, activation)| **activation >= 0.5)
        .map(|(frame, _)| {
            frames
                .iter()
                .map(|selected| frame.abs_diff(*selected))
                .min()
                .is_some_and(|distance| distance <= 2)
        })
        .filter(|supported| *supported)
        .count() as f32
        / beat
            .iter()
            .filter(|activation| **activation >= 0.5)
            .count()
            .max(1) as f32;
    if coverage < MIN_COVERAGE || support < 0.55 || residual > 0.28 {
        return None;
    }
    let activation_mean =
        frames.iter().map(|frame| beat[*frame]).sum::<f32>() / frames.len().max(1) as f32;
    if activation_mean < 0.20 {
        return None;
    }
    let beat_confidence = ((0.45 * coverage + 0.35 * activation_mean + 0.20 * (1.0 - residual))
        * (0.55 + 0.45 * alias_margin))
        .clamp(0.0, 1.0);
    let marker_confidences = frames
        .iter()
        .enumerate()
        .map(|(index, frame)| {
            // Compare this selected node with nearby nodes that could have
            // occupied the same bounded-DP transition. This is a local
            // runner-up margin, rather than a cumulative path score, so later
            // markers do not become artificially more certain.
            let previous = index.checked_sub(1).map(|index| frames[index]);
            let selected_score = previous.map_or(beat[*frame] - START_PENALTY, |previous| {
                let interval = frame.saturating_sub(previous);
                let continuity = -0.045 * (interval as f32 - period as f32).abs();
                beat[*frame] - 0.44 + continuity
            });
            let runner_up = beat
                .iter()
                .enumerate()
                .filter(|(candidate, _)| {
                    *candidate != *frame
                        && candidate.abs_diff(*frame) <= 3
                        && previous.is_none_or(|previous| {
                            let interval = candidate.saturating_sub(previous);
                            period_bounds(observations.frame_rate_hz).is_some_and(
                                |(minimum, maximum)| (minimum..=maximum).contains(&interval),
                            )
                        })
                })
                .map(|(candidate, activation)| {
                    previous.map_or(*activation - START_PENALTY, |previous| {
                        let interval = candidate.saturating_sub(previous);
                        let continuity = -0.045 * (interval as f32 - period as f32).abs();
                        *activation - 0.44 + continuity
                    })
                })
                .fold(f32::NEG_INFINITY, f32::max);
            let dp_margin = (selected_score - runner_up.max(0.0)).clamp(0.0, 1.0);
            let continuity = if index == 0 {
                1.0
            } else {
                (1.0 - (intervals[index - 1] as f32 - period as f32).abs() / period as f32)
                    .clamp(0.0, 1.0)
            };
            let confidence =
                (0.55 * beat[*frame] + 0.25 * dp_margin + 0.20 * continuity).clamp(0.0, 1.0);
            // A continuity-only backfill is useful for following a missing
            // onset, but must never become a trusted kick marker.
            if beat[*frame] < 0.5 {
                confidence.min(0.20)
            } else {
                confidence
            }
        })
        .collect::<Vec<_>>();

    let mut phase_scores = [0.0_f32; 4];
    for (phase, phase_score) in phase_scores.iter_mut().enumerate() {
        let values = frames
            .iter()
            .enumerate()
            .filter(|(index, _)| index % 4 == phase)
            .map(|(_, frame)| downbeat[*frame]);
        let values = values.collect::<Vec<_>>();
        *phase_score = if values.is_empty() {
            0.0
        } else {
            let mut values = values;
            values.sort_by(f32::total_cmp);
            let from = values.len() / 2;
            values[from..].iter().sum::<f32>() / (values.len() - from).max(1) as f32
        };
    }
    let mut ranked = phase_scores.into_iter().enumerate().collect::<Vec<_>>();
    ranked.sort_by(|left, right| right.1.total_cmp(&left.1));
    let best_phase = ranked[0].0;
    let margin = ranked[0].1 - ranked[1].1;
    let normalized_margin = margin / ranked[0].1.max(f32::EPSILON);
    let phase_count = frames
        .iter()
        .enumerate()
        .filter(|(index, _)| index % 4 == best_phase)
        .count();
    let phase_support = frames
        .iter()
        .enumerate()
        .filter(|(index, frame)| index % 4 == best_phase && downbeat[**frame] >= 0.5)
        .count() as f32
        / phase_count.max(1) as f32;
    let downbeat_coverage = frames
        .iter()
        .filter(|frame| downbeat[**frame] >= 0.5)
        .count() as f32
        / frames.len().max(1) as f32;
    let downbeat_confidence = if ranked[0].1 >= 0.30
        && margin >= MIN_DOWNBEAT_MARGIN
        && normalized_margin >= MIN_NORMALIZED_DOWNBEAT_MARGIN
        && phase_support >= 0.5
        && downbeat_coverage >= 0.15
    {
        (0.65 * ranked[0].1 + 0.35 * (margin / ranked[0].1.max(f32::EPSILON))).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let first_downbeat_ordinal = (downbeat_confidence > 0.0).then(|| {
        frames
            .iter()
            .enumerate()
            .find(|(index, _)| index % 4 == best_phase)
            .map(|(index, _)| index)
            .unwrap_or(best_phase.min(frames.len().saturating_sub(1)))
    });

    Some(DecodedGrid {
        frames,
        marker_confidences,
        period_frames: period,
        bpm,
        beat_confidence,
        first_downbeat_ordinal,
        downbeat_confidence,
    })
}

#[derive(Clone, Copy)]
struct DpState {
    score: f32,
    predecessor: Option<usize>,
    period: usize,
}

fn period_bounds(frame_rate_hz: f32) -> Option<(usize, usize)> {
    if !frame_rate_hz.is_finite() || frame_rate_hz <= 0.0 {
        return None;
    }
    // An integer frame period is valid when its implied BPM is in the
    // decoder range. Ceil/floor keep both boundaries fail-closed. Avoid a
    // giant allocation/search window for hostile metadata even if a caller
    // supplies an otherwise finite frame rate.
    let minimum = (frame_rate_hz * 60.0 / MAX_BPM).ceil();
    let maximum = (frame_rate_hz * 60.0 / MIN_BPM).floor();
    if !minimum.is_finite()
        || !maximum.is_finite()
        || minimum < 1.0
        || maximum < minimum
        || maximum > usize::MAX as f32
    {
        return None;
    }
    Some((minimum as usize, maximum as usize))
}

fn bounded_dp(activations: &[f32], frame_rate_hz: f32) -> Option<Vec<usize>> {
    let (minimum_period, maximum_period) = period_bounds(frame_rate_hz)?;
    let mut states: Vec<Option<DpState>> = vec![None; activations.len()];
    for current in 0..activations.len() {
        let activation_reward = activations[current] - 0.44;
        let mut best = DpState {
            // Starting a fresh path is more expensive than extending a
            // supported path. This prevents a low-activation local burst from
            // winning solely because it can form four legal intervals.
            score: activation_reward - START_PENALTY,
            predecessor: None,
            period: 0,
        };
        let from = current.saturating_sub(maximum_period);
        let to = current.saturating_sub(minimum_period);
        for (predecessor, previous) in states.iter().enumerate().take(to + 1).skip(from) {
            let Some(previous) = *previous else {
                continue;
            };
            let period = current - predecessor;
            let continuity = if previous.period == 0 {
                0.0
            } else {
                -0.045 * (period as f32 - previous.period as f32).abs()
            };
            let score = previous.score + activation_reward + continuity;
            if score > best.score {
                best = DpState {
                    score,
                    predecessor: Some(predecessor),
                    period,
                };
            }
        }
        states[current] = Some(best);
    }
    let (end, _) = states
        .iter()
        .enumerate()
        .filter_map(|(index, state)| state.map(|state| (index, state)))
        .max_by(|left, right| left.1.score.total_cmp(&right.1.score))?;
    let mut frames = Vec::new();
    let mut cursor = Some(end);
    while let Some(index) = cursor {
        let state = states[index]?;
        frames.push(index);
        cursor = state.predecessor;
    }
    frames.reverse();
    (frames.len() >= MIN_PATH_MARKERS).then_some(frames)
}

fn robust_period(intervals: &[usize], frame_rate_hz: f32) -> Option<usize> {
    if intervals.is_empty() {
        return None;
    }
    let mut sorted = intervals.to_vec();
    sorted.sort_unstable();
    let median = sorted[sorted.len() / 2];
    let (minimum_period, maximum_period) = period_bounds(frame_rate_hz)?;
    if !(minimum_period..=maximum_period).contains(&median) {
        return None;
    }
    // A period with a large MAD is usually non-periodic material or a false
    // half/double-time path.  Keep one outlier for missing beats, but reject a
    // majority of incompatible intervals.
    let mad = sorted
        .iter()
        .map(|interval| (*interval as f32 - median as f32).abs())
        .sum::<f32>()
        / sorted.len() as f32;
    (mad <= median as f32 * 0.28).then_some(median)
}

/// Compare explicit half, native, and double-time hypotheses.  The bounded DP
/// path remains the source of marker positions, while this evidence controls
/// tempo confidence; ambiguous aliases therefore become beat-only candidates
/// instead of silently claiming a strong tempo.
fn alias_hypothesis_margin(
    activations: &[f32],
    frames: &[usize],
    period: usize,
    frame_rate_hz: f32,
) -> f32 {
    let Some((minimum_period, maximum_period)) = period_bounds(frame_rate_hz) else {
        return 0.0;
    };
    let mut candidates = Vec::with_capacity(3);
    let anchor = frames.first().copied().unwrap_or_default() as f32;
    for candidate in [period as f32 / 2.0, period as f32, period as f32 * 2.0] {
        if !(minimum_period as f32..=maximum_period as f32).contains(&candidate) {
            continue;
        }
        let observed = activations
            .iter()
            .enumerate()
            .filter(|(_, activation)| **activation >= 0.5)
            .collect::<Vec<_>>();
        let support = observed
            .iter()
            .map(|(frame, _)| {
                let step = ((*frame as f32 - anchor) / candidate).round();
                let expected = anchor + step * candidate;
                if (*frame as f32 - expected).abs() <= 2.0 {
                    1.0
                } else {
                    0.0
                }
            })
            .sum::<f32>()
            / observed.len().max(1) as f32;
        let activation =
            observed.iter().map(|(_, value)| **value).sum::<f32>() / observed.len().max(1) as f32;
        let continuity = frames
            .windows(2)
            .map(|pair| 1.0 - (pair[1] as f32 - pair[0] as f32 - candidate).abs() / candidate)
            .map(|value| value.clamp(0.0, 1.0))
            .sum::<f32>()
            / frames.len().saturating_sub(1).max(1) as f32;
        // Explicitly compare 0.5x/1x/2x on observed coverage, activation,
        // and local interval continuity. Do not resolve aliases by choosing
        // whichever BPM is numerically closest to 120.
        candidates.push(0.50 * support + 0.25 * activation + 0.25 * continuity);
    }
    candidates.sort_by(f32::total_cmp);
    let best = candidates.last().copied().unwrap_or_default();
    let runner_up = candidates.iter().rev().nth(1).copied().unwrap_or_default();
    (best - runner_up).clamp(0.0, 1.0)
}

/// Build the explicit half/native/double-time family for the selected path.
///
/// The path itself is always the native (relative multiplier `1.0`) member.
/// Alias candidates are scored against the observed activations and path
/// intervals, then normalized as relative weights. This metadata does not
/// synthesize or replace `beat_events`.
fn tempo_hypotheses(
    observations: &NeuralBeatObservations,
    frames: &[usize],
    period: usize,
) -> Vec<TempoHypothesis> {
    let Some((minimum_period, maximum_period)) = period_bounds(observations.frame_rate_hz) else {
        return Vec::new();
    };
    if period == 0 || frames.is_empty() {
        return Vec::new();
    }
    let beat = observations
        .beat_logits
        .iter()
        .copied()
        .map(logit_probability)
        .collect::<Vec<_>>();
    let observed = beat
        .iter()
        .enumerate()
        .filter(|(_, activation)| **activation >= 0.5)
        .collect::<Vec<_>>();
    let anchor = frames.first().copied().unwrap_or_default() as f32;
    let primary_bpm = 60.0 * observations.frame_rate_hz / period as f32;
    let mut candidates = Vec::with_capacity(3);
    // Keep the selected path first. Consumers can therefore use the first
    // candidate as the primary tempo without reconstructing it from aliases.
    for (relation, multiplier, candidate_period) in [
        (TempoRelation::Primary, 1.0_f32, period as f32),
        (TempoRelation::HalfTime, 0.5_f32, period as f32 * 2.0),
        (TempoRelation::DoubleTime, 2.0_f32, period as f32 / 2.0),
    ] {
        let bpm = primary_bpm * multiplier;
        if !bpm.is_finite()
            || !(MIN_BPM..=MAX_BPM).contains(&bpm)
            || !candidate_period.is_finite()
            || candidate_period < minimum_period as f32
            || candidate_period > maximum_period as f32
        {
            continue;
        }
        let support = observed
            .iter()
            .map(|(frame, _)| {
                let step = ((*frame as f32 - anchor) / candidate_period).round();
                let expected = anchor + step * candidate_period;
                if (*frame as f32 - expected).abs() <= 2.0 {
                    1.0
                } else {
                    0.0
                }
            })
            .sum::<f32>()
            / observed.len().max(1) as f32;
        let activation =
            observed.iter().map(|(_, value)| **value).sum::<f32>() / observed.len().max(1) as f32;
        let continuity = frames
            .windows(2)
            .map(|pair| {
                1.0 - (pair[1] as f32 - pair[0] as f32 - candidate_period).abs()
                    / candidate_period.max(f32::EPSILON)
            })
            .map(|value| value.clamp(0.0, 1.0))
            .sum::<f32>()
            / frames.len().saturating_sub(1).max(1) as f32;
        let score = (0.50 * support + 0.25 * activation + 0.25 * continuity).clamp(0.0, 1.0);
        candidates.push((bpm, score.max(f32::EPSILON), relation));
    }
    if candidates.is_empty() {
        return Vec::new();
    }
    let total = candidates.iter().map(|(_, score, _)| *score).sum::<f32>();
    if !total.is_finite() || total <= f32::EPSILON {
        return Vec::new();
    }
    let mut hypotheses = candidates
        .into_iter()
        .map(|(bpm, score, relation)| TempoHypothesis {
            bpm,
            relative_weight: score / total,
            relation,
        })
        .collect::<Vec<_>>();
    // Correct the final representable-f32 rounding residue so callers that
    // validate the normalized family do not observe a sum such as 0.99999994.
    if hypotheses.len() > 1 {
        let preceding = hypotheses[..hypotheses.len() - 1]
            .iter()
            .map(|hypothesis| hypothesis.relative_weight)
            .sum::<f32>();
        hypotheses.last_mut().unwrap().relative_weight = (1.0 - preceding).clamp(0.0, 1.0);
    } else {
        hypotheses[0].relative_weight = 1.0;
    }
    hypotheses
}

fn logit_probability(value: f32) -> f32 {
    if !value.is_finite() {
        return 0.0;
    }
    let value = value.clamp(-20.0, 20.0);
    1.0 / (1.0 + (-value).exp())
}

fn low_band_onset(low_band: &[f32]) -> Vec<f32> {
    if low_band.len() < 4 {
        return Vec::new();
    }
    // Use short-window power flux rather than sample-to-sample absolute
    // differences. The latter treats a one-sample impulse and high-frequency
    // bleed as a kick onset and is particularly unstable around frame edges.
    let window = 10_usize;
    let mut power = vec![0.0_f32; low_band.len()];
    let mut sum = 0.0_f32;
    for (index, sample) in low_band.iter().copied().enumerate() {
        let sample = if sample.is_finite() {
            sample.clamp(-1_000.0, 1_000.0)
        } else {
            0.0
        };
        sum += sample * sample;
        if index >= window {
            let removed = if low_band[index - window].is_finite() {
                low_band[index - window].clamp(-1_000.0, 1_000.0)
            } else {
                0.0
            };
            sum -= removed * removed;
        }
        let length = (index + 1).min(window);
        power[index] = (sum.max(0.0) / length as f32).sqrt();
    }
    let mut flux = vec![0.0_f32; low_band.len()];
    for index in 1..power.len() {
        flux[index] = (power[index] - power[index - 1]).max(0.0);
    }
    // A three-millisecond box smooths codec/sample jitter without moving the
    // candidate away from the source-rate onset clock.
    (0..flux.len())
        .map(|index| {
            let from = index.saturating_sub(1);
            let to = (index + 2).min(flux.len());
            flux[from..to].iter().sum::<f32>() / (to - from).max(1) as f32
        })
        .collect()
}

fn refined_bpm(markers: &[Duration]) -> Option<f32> {
    let mut intervals = markers
        .windows(2)
        .map(|pair| pair[1].as_secs_f32() - pair[0].as_secs_f32())
        .filter(|interval| interval.is_finite() && *interval > 0.0)
        .collect::<Vec<_>>();
    if intervals.is_empty() {
        return None;
    }
    intervals.sort_by(f32::total_cmp);
    let interval = intervals[intervals.len() / 2];
    let bpm = 60.0 / interval;
    (MIN_BPM..=MAX_BPM).contains(&bpm).then_some(bpm)
}

fn refine_markers(
    frames: &[usize],
    low_onset: &[f32],
    frame_rate_hz: f32,
) -> (Vec<usize>, Vec<f32>) {
    // Refinement returns only timestamps and low-frequency support. Model
    // scores and timing confidence stay on `DecodedGrid` so a kick-band
    // search can never rewrite confidence or remove a neural event.
    if low_onset.is_empty() {
        return (
            frames
                .iter()
                .map(|frame| (*frame as f32 / frame_rate_hz * 1_000.0).round() as usize)
                .collect(),
            vec![0.0; frames.len()],
        );
    }
    let half_window = (REFINE_WINDOW.as_secs_f32() * 1_000.0).round() as isize;
    let min_spacing_ms = MIN_REFINED_SPACING.as_millis() as usize;
    let maximum = low_onset
        .iter()
        .copied()
        .filter(|value| value.is_finite())
        .fold(0.0_f32, f32::max);
    let mut baseline = low_onset
        .iter()
        .copied()
        .filter(|value| value.is_finite())
        .collect::<Vec<_>>();
    baseline.sort_by(f32::total_cmp);
    let noise_floor = baseline
        .get(baseline.len() / 2)
        .copied()
        .unwrap_or_default();
    // A zero/silent profile must not select the left edge of every search
    // window merely because every sample satisfies `>= 0`. Require both a
    // relative onset level and a floor above the local noise median.
    let reference = (maximum * 0.06).max(noise_floor * 1.5).max(f32::EPSILON);
    let mut previous: Option<usize> = None;
    let mut refined_samples = Vec::with_capacity(frames.len());
    let mut refined_support = Vec::with_capacity(frames.len());
    for frame in frames {
        let center = (*frame as f32 / frame_rate_hz * 1_000.0).round() as isize;
        let from = (center - half_window).max(1) as usize;
        let to = (center + half_window).min(low_onset.len().saturating_sub(1) as isize) as usize;
        let candidate = (from..=to)
            .filter(|index| low_onset[*index] >= reference)
            .fold(None, |best: Option<usize>, index| {
                let Some(best_index) = best else {
                    return Some(index);
                };
                let value = low_onset[index];
                let best_value = low_onset[best_index];
                let closer = (index as isize - center).unsigned_abs()
                    < (best_index as isize - center).unsigned_abs();
                (value > best_value + f32::EPSILON
                    || ((value - best_value).abs() <= f32::EPSILON && closer))
                    .then_some(index)
                    .or(Some(best_index))
            });
        if let Some(index) = candidate
            && previous.is_none_or(|previous| index >= previous.saturating_add(min_spacing_ms))
        {
            previous = Some(index);
            refined_samples.push(index);
            refined_support.push(
                ((low_onset[index] - noise_floor) / (maximum - noise_floor + f32::EPSILON))
                    .clamp(0.0, 1.0),
            );
            continue;
        }
        let fallback = center.max(0) as usize;
        previous = Some(fallback);
        refined_samples.push(fallback);
        refined_support.push(0.0);
    }
    (refined_samples, refined_support)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn click_logits(period: usize, length: usize) -> NeuralBeatObservations {
        click_logits_at_rate(FRAME_RATE, period, length)
    }

    fn click_logits_at_rate(
        frame_rate_hz: f32,
        period: usize,
        length: usize,
    ) -> NeuralBeatObservations {
        let mut beat = vec![-7.0; length];
        let mut downbeat = vec![-7.0; length];
        for frame in (25..length).step_by(period) {
            beat[frame] = 6.0;
            if ((frame - 25) / period).is_multiple_of(4) {
                downbeat[frame] = 6.0;
            }
        }
        NeuralBeatObservations::with_frame_rate(frame_rate_hz, beat, downbeat).unwrap()
    }

    #[test]
    fn bounded_dp_tracks_four_on_the_floor_and_downbeat_phase() {
        let observations = click_logits(25, 600);
        let grid = decode_grid(&observations).expect("periodic logits should decode");
        assert!((grid.bpm - 120.0).abs() < 1.0, "{}", grid.bpm);
        assert!(grid.beat_confidence > 0.6);
        assert!(grid.downbeat_confidence > 0.2);
        assert_eq!(grid.first_downbeat_ordinal, Some(0));
    }

    #[test]
    fn v2_decode_api_returns_event_timeline_domain_value() {
        let observations = click_logits(25, 600);
        let rhythm = decode_neural_rhythm_analysis(&observations).expect("V2 rhythm decode");
        assert!(!rhythm.beats.is_empty());
        assert!(rhythm.validate());
        assert!(rhythm.primary_tempo().is_some());
        assert!(
            rhythm
                .beats
                .windows(2)
                .all(|window| window[0].time < window[1].time)
        );
    }

    #[test]
    fn silence_and_nonperiodic_logits_are_rejected() {
        let silence = NeuralBeatObservations::new(vec![-7.0; 400], vec![-7.0; 400]).unwrap();
        assert!(decode_grid(&silence).is_none());
        let mut beat = vec![-7.0; 600];
        for frame in [20, 59, 101, 133, 188, 251, 300, 367, 411, 512] {
            beat[frame] = 7.0;
        }
        let observations = NeuralBeatObservations::new(beat, vec![-7.0; 600]).unwrap();
        assert!(decode_grid(&observations).is_none());
    }

    #[test]
    fn refinement_is_bounded_and_keeps_missing_marker_on_grid() {
        let observations = click_logits(25, 300);
        let mut analysis = TrackAnalysis::unanalyzed(Duration::from_secs(6));
        analysis.audible_end = Duration::from_secs(6);
        let mut low = vec![0.0; 6_000];
        for marker in [503, 1_000, 1_503, 2_000, 2_503, 3_000] {
            low[marker] = 1.0;
        }
        assert!(apply_neural_beat_observations(
            &mut analysis,
            &observations,
            &low
        ));
        assert!(
            analysis
                .beat_markers
                .windows(2)
                .all(|pair| pair[1] > pair[0])
        );
        assert!(
            analysis
                .beat_markers
                .iter()
                .all(|marker| { marker.as_secs_f32() >= 0.0 && marker.as_secs_f32() <= 6.0 })
        );
        assert!(
            analysis
                .beat_markers
                .iter()
                .any(|marker| marker.as_millis() % 20 != 0)
        );
        assert!(
            analysis
                .beat_marker_confidences
                .iter()
                .any(|confidence| *confidence >= 0.35),
            "low-band-supported markers should be eligible for trusted phase use"
        );
    }

    #[test]
    fn raw_model_peaks_without_low_band_support_stay_untrusted() {
        let observations = click_logits(25, 300);
        let mut analysis = TrackAnalysis::unanalyzed(Duration::from_secs(6));
        analysis.audible_end = Duration::from_secs(6);
        let low = vec![0.0; 6_000];
        assert!(apply_neural_beat_observations(
            &mut analysis,
            &observations,
            &low
        ));
        assert!(
            analysis
                .beat_marker_confidences
                .iter()
                .all(|confidence| *confidence < 0.35),
            "model-only peaks must not become trusted kick markers"
        );
        assert_eq!(analysis.trusted_kick_coverage(), 0.0);
    }

    #[test]
    fn bounded_dp_follows_gradual_tempo_drift_and_one_missing_beat() {
        let mut beat = vec![-7.0; 1_200];
        let mut downbeat = vec![-7.0; 1_200];
        let mut frame = 25_usize;
        let mut count = 0_usize;
        while frame < beat.len() {
            beat[frame] = 6.0;
            if count.is_multiple_of(4) {
                downbeat[frame] = 5.0;
            }
            count += 1;
            frame += 25 + (count / 10).min(4);
        }
        // One missing model peak should be represented by the continuity path.
        let missing = 12 * 25 + 25;
        beat[missing] = -7.0;
        let observations = NeuralBeatObservations::new(beat, downbeat).unwrap();
        let grid = decode_grid(&observations).expect("drifting periodic logits should decode");
        assert!(grid.frames.len() > 20);
        assert!((grid.bpm - 113.0).abs() < 8.0, "{}", grid.bpm);
    }

    #[test]
    fn pickup_before_audible_start_does_not_shift_downbeat_ordinal() {
        let mut beat = vec![-7.0; 500];
        let mut downbeat = vec![-7.0; 500];
        for (index, frame) in (10..500).step_by(25).enumerate() {
            beat[frame] = 6.0;
            if index % 4 == 0 {
                downbeat[frame] = 6.0;
            }
        }
        let observations = NeuralBeatObservations::new(beat, downbeat).unwrap();
        let mut analysis = TrackAnalysis::unanalyzed(Duration::from_secs(10));
        analysis.audible_start = Duration::from_millis(400);
        analysis.audible_end = Duration::from_secs(10);
        assert!(apply_neural_beat_observations(
            &mut analysis,
            &observations,
            &vec![0.0; 10_000]
        ));
        assert!(
            analysis
                .first_beat
                .is_some_and(|beat| beat >= analysis.audible_start)
        );
        assert!(analysis.first_downbeat.is_some_and(|downbeat| {
            downbeat >= analysis.audible_start && downbeat > analysis.first_beat.unwrap()
        }));
    }

    #[test]
    fn alias_hypotheses_use_coverage_and_interval_evidence() {
        let frames = (20..700).step_by(34).collect::<Vec<_>>();
        let mut activations = vec![0.01; 800];
        for frame in &frames {
            activations[*frame] = 0.99;
        }
        let clear_native = alias_hypothesis_margin(&activations, &frames, 34, 50.0);
        assert!(clear_native > 0.1, "alias margin={clear_native}");

        // A competing 0.5x sequence receives coverage but loses interval
        // continuity; this guards against selecting aliases by BPM proximity.
        let half_frames = (20..700).step_by(17).collect::<Vec<_>>();
        let ambiguous = alias_hypothesis_margin(&activations, &half_frames, 17, 50.0);
        assert!(ambiguous.is_finite());
    }

    #[test]
    fn bounded_dp_accepts_tempo_family_edges_without_forcing_a_120_bpm_alias() {
        for bpm in [90.0_f32, 100.0, 130.0, 180.0] {
            let period = (50.0 * 60.0 / bpm).round() as usize;
            let observations = click_logits(period, 900);
            let grid = decode_grid(&observations).expect("tempo family should decode");
            let expected = 3_000.0 / period as f32;
            assert!((grid.bpm - expected).abs() < 1.5, "bpm={bpm} grid={grid:?}");
        }
    }

    #[test]
    fn bounded_dp_uses_observation_rate_for_60_and_220_bpm_edges() {
        // 44 Hz makes both edge periods integral (44 frames = 60 BPM and
        // 12 frames = 220 BPM), so this also catches accidental 50 Hz math.
        for (bpm, period) in [(60.0_f32, 44_usize), (220.0_f32, 12_usize)] {
            let observations = click_logits_at_rate(44.0, period, 700);
            let grid = decode_grid(&observations).expect("edge tempo should decode");
            assert!((grid.bpm - bpm).abs() < 0.1, "bpm={bpm} grid={grid:?}");
        }
    }

    #[test]
    fn rhythm_result_retains_primary_path_and_normalized_85_170_family() {
        let observations = click_logits(35, 900);
        let rhythm = decode_neural_rhythm(&observations).expect("periodic logits should decode");
        assert!((rhythm.primary_bpm - 85.7).abs() < 1.0);
        assert!(rhythm.beat_events.len() > 12);
        assert_eq!(
            rhythm.beat_events.len(),
            decode_neural_grid(&observations).unwrap().len()
        );
        assert!(
            rhythm
                .tempo_hypotheses
                .iter()
                .any(|hypothesis| (hypothesis.bpm - rhythm.primary_bpm).abs() < 0.1)
        );
        assert!(
            rhythm
                .tempo_hypotheses
                .iter()
                .any(|hypothesis| (hypothesis.bpm - rhythm.primary_bpm * 2.0).abs() < 0.2)
        );
        let total = rhythm
            .tempo_hypotheses
            .iter()
            .map(|hypothesis| hypothesis.relative_weight)
            .sum::<f32>();
        assert!(
            (total - 1.0).abs() < 1e-6,
            "weights={:?}",
            rhythm.tempo_hypotheses
        );
    }

    #[test]
    fn model_only_rhythm_keeps_low_frequency_support_unknown() {
        let observations = click_logits(25, 600);
        let rhythm = decode_neural_rhythm(&observations).expect("periodic logits should decode");
        assert!(
            rhythm
                .beat_events
                .iter()
                .all(|event| event.low_frequency_support.is_none())
        );
    }

    #[test]
    fn supplied_empty_low_band_is_explicit_zero_support() {
        let observations = click_logits(25, 600);
        let rhythm = decode_neural_rhythm_with_low_frequency(&observations, &[])
            .expect("empty low-frequency stream should retain model events");
        assert!(
            rhythm
                .beat_events
                .iter()
                .all(|event| event.low_frequency_support == Some(0.0))
        );
    }

    #[test]
    fn refinement_does_not_change_model_or_timing_scores_or_drop_events() {
        let observations = click_logits(25, 600);
        let base = decode_neural_rhythm(&observations).expect("periodic logits should decode");
        let mut low = vec![0.0_f32; 12_000];
        for event in &base.beat_events {
            let sample = event.time.as_millis() as usize;
            if let Some(value) = low.get_mut(sample.saturating_add(7)) {
                *value = 0.2;
            }
        }
        let refined = decode_neural_rhythm_with_low_frequency(&observations, &low)
            .expect("low-frequency refinement should preserve decode");
        assert_eq!(refined.beat_events.len(), base.beat_events.len());
        for (before, after) in base.beat_events.iter().zip(&refined.beat_events) {
            assert_eq!(before.model_score, after.model_score);
            assert_eq!(before.onset_support, after.onset_support);
            assert_eq!(before.timing_confidence, after.timing_confidence);
        }
        assert!(refined.beat_events.iter().any(|event| {
            event
                .low_frequency_support
                .is_some_and(|support| support > 0.0)
        }));
    }

    #[test]
    fn malformed_logits_do_not_panic_or_create_a_grid() {
        let mut beat = vec![-7.0; 16];
        beat[..3].copy_from_slice(&[f32::NAN, f32::INFINITY, -f32::INFINITY]);
        let observations = NeuralBeatObservations::new(beat, vec![-7.0; 16]).unwrap();
        assert!(decode_grid(&observations).is_none());
    }

    #[test]
    fn downbeat_phase_with_multiple_high_candidates_is_beat_only() {
        let mut beat = vec![-7.0; 500];
        let mut downbeat = vec![-7.0; 500];
        for frame in (25..500).step_by(25) {
            beat[frame] = 6.0;
        }
        for (phase, logit) in [6.0, 2.2, 2.0, 1.9].into_iter().enumerate() {
            for frame in (25 + phase * 25..500).step_by(100) {
                downbeat[frame] = logit;
            }
        }
        let observations = NeuralBeatObservations::new(beat, downbeat).unwrap();
        let grid = decode_grid(&observations).expect("beat grid should survive ambiguity");
        assert!(grid.downbeat_confidence < 0.6);
        assert!(grid.first_downbeat_ordinal.is_none());
    }

    #[test]
    fn failed_neural_decode_preserves_classical_analysis_for_fallback() {
        let observations = NeuralBeatObservations::new(vec![-7.0; 240], vec![-7.0; 240]).unwrap();
        let mut analysis = TrackAnalysis::unanalyzed(Duration::from_secs(5));
        analysis.bpm = Some(128.0);
        analysis.beat_confidence = 0.7;
        let before = analysis.clone();
        assert!(!apply_neural_beat_observations(
            &mut analysis,
            &observations,
            &[0.0; 5_000]
        ));
        assert_eq!(analysis, before);
    }
}
