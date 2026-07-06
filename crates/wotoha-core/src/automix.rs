use std::time::Duration;

pub const TEMPO_SYNC_DEADBAND: f32 = 0.001;
const MAX_TEMPO_SEGMENTS: usize = 32;

#[derive(Clone, Debug, PartialEq)]
pub struct AutoMixConfig {
    pub enabled: bool,
    pub crossfade: Duration,
    pub max_tempo_adjustment: f32,
    pub min_beat_confidence: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TrackAnalysis {
    pub duration: Duration,
    pub audible_start: Duration,
    pub audible_end: Duration,
    pub bpm: Option<f32>,
    pub beat_confidence: f32,
    pub first_beat: Option<Duration>,
    /// Detected beat onsets used to follow local tempo changes during a transition.
    pub beat_markers: Vec<Duration>,
    pub first_downbeat: Option<Duration>,
    pub downbeat_confidence: f32,
    pub musical_key: Option<MusicalKey>,
    /// Unweighted full-band RMS level. This is dBFS, not LUFS.
    pub rms_dbfs: Option<f32>,
    pub sample_peak_dbfs: Option<f32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyMode {
    Major,
    Minor,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MusicalKey {
    /// Pitch class where C=0, C#=1, ... B=11.
    pub tonic: u8,
    pub mode: KeyMode,
    pub confidence: f32,
}

/// A lightweight 4/4 beat grid inferred from tempo and onset accents.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BeatGrid {
    pub first_downbeat: Duration,
    pub beat_interval: Duration,
    pub beats_per_bar: u8,
    pub downbeat_confidence: f32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum PhraseLength {
    FourBars,
    EightBars,
    SixteenBars,
}

impl PhraseLength {
    pub const fn bars(self) -> u32 {
        match self {
            Self::FourBars => 4,
            Self::EightBars => 8,
            Self::SixteenBars => 16,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhraseCue {
    pub position: Duration,
    pub length: PhraseLength,
}

impl TrackAnalysis {
    pub fn unanalyzed(duration: Duration) -> Self {
        Self {
            duration,
            audible_start: Duration::ZERO,
            audible_end: duration,
            bpm: None,
            beat_confidence: 0.0,
            first_beat: None,
            beat_markers: Vec::new(),
            first_downbeat: None,
            downbeat_confidence: 0.0,
            musical_key: None,
            rms_dbfs: None,
            sample_peak_dbfs: None,
        }
    }

    pub fn beat_grid(&self) -> Option<BeatGrid> {
        let first_downbeat = self.first_downbeat?;
        let bpm = self.bpm?;
        if bpm <= 0.0 || !bpm.is_finite() || self.beat_confidence <= 0.0 {
            return None;
        }
        let beat_interval = Duration::from_secs_f32(60.0 / bpm);
        (!beat_interval.is_zero()).then_some(BeatGrid {
            first_downbeat,
            beat_interval,
            beats_per_bar: 4,
            downbeat_confidence: self.downbeat_confidence,
        })
    }

    /// Returns inferred 4/8/16-bar boundaries inside the audible region.
    pub fn phrase_cues(&self) -> Vec<PhraseCue> {
        let Some(grid) = self.beat_grid() else {
            return Vec::new();
        };
        if grid.downbeat_confidence < 0.25 {
            return Vec::new();
        }
        let mut cues = Vec::new();
        for length in [
            PhraseLength::FourBars,
            PhraseLength::EightBars,
            PhraseLength::SixteenBars,
        ] {
            let phrase = grid
                .beat_interval
                .mul_f64((grid.beats_per_bar as u32 * length.bars()) as f64);
            if phrase.is_zero() {
                continue;
            }
            let mut position = grid.first_downbeat;
            while position < self.audible_start {
                position += phrase;
            }
            while position <= self.audible_end {
                cues.push(PhraseCue { position, length });
                position += phrase;
            }
        }
        cues.sort_by_key(|cue| cue.position);
        cues
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransitionKind {
    Gapless,
    Crossfade,
    BeatMatched,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TransitionPlan {
    pub kind: TransitionKind,
    pub outgoing_start: Duration,
    pub incoming_start: Duration,
    pub duration: Duration,
    /// Playback speed applied to the incoming deck. `1.0` preserves its tempo.
    pub incoming_tempo_ratio: f32,
    pub harmonic_compatibility: Option<f32>,
    /// Relative gain retained by the incoming track after the overlap.
    pub incoming_gain: f32,
    pub tempo_envelope: Option<TempoEnvelope>,
}

/// Maps output time to source time while the incoming deck returns to native tempo.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TempoEnvelope {
    pub initial_speed: f32,
    /// Target speed reached at the end of the overlap.
    pub mix_end_speed: f32,
    pub hold: Duration,
    pub ramp: Duration,
    phase_segments: [TempoSegment; MAX_TEMPO_SEGMENTS],
    phase_segment_count: u8,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct TempoSegment {
    output_end: Duration,
    speed: f32,
}

impl TempoSegment {
    const EMPTY: Self = Self {
        output_end: Duration::ZERO,
        speed: 1.0,
    };
}

impl TempoEnvelope {
    pub fn new(initial_speed: f32, mix_end_speed: f32, hold: Duration, ramp: Duration) -> Self {
        Self {
            initial_speed,
            mix_end_speed,
            hold,
            ramp,
            phase_segments: [TempoSegment::EMPTY; MAX_TEMPO_SEGMENTS],
            phase_segment_count: 0,
        }
    }

    fn with_phase_segments(mut self, segments: &[TempoSegment]) -> Self {
        let count = segments.len().min(MAX_TEMPO_SEGMENTS);
        self.phase_segments[..count].copy_from_slice(&segments[..count]);
        self.phase_segment_count = count as u8;
        self
    }

    pub fn speed_at(self, output_elapsed: Duration) -> f32 {
        if output_elapsed <= self.hold {
            for segment in &self.phase_segments[..usize::from(self.phase_segment_count)] {
                if output_elapsed <= segment.output_end {
                    return segment.speed;
                }
            }
            if self.phase_segment_count > 0 {
                return self.mix_end_speed;
            }
            if self.hold.is_zero() {
                return self.mix_end_speed;
            }
            let progress = output_elapsed.as_secs_f32() / self.hold.as_secs_f32();
            return self.initial_speed
                + (self.mix_end_speed - self.initial_speed) * progress.clamp(0.0, 1.0);
        }
        if self.ramp.is_zero() {
            return 1.0;
        }
        let ramp_elapsed = output_elapsed.saturating_sub(self.hold);
        if ramp_elapsed >= self.ramp {
            return 1.0;
        }
        let progress = ramp_elapsed.as_secs_f32() / self.ramp.as_secs_f32();
        self.mix_end_speed + (1.0 - self.mix_end_speed) * progress
    }

    pub fn source_elapsed(self, output_elapsed: Duration) -> Duration {
        let output = output_elapsed.as_secs_f64();
        let hold = self.hold.as_secs_f64();
        let ramp = self.ramp.as_secs_f64();
        let initial = f64::from(self.initial_speed);
        let mix_end = f64::from(self.mix_end_speed);
        let hold_source = self.hold_source_elapsed();
        let source = if output <= hold {
            if self.phase_segment_count > 0 {
                self.segmented_source_elapsed(output_elapsed).as_secs_f64()
            } else if hold > 0.0 {
                initial * output + 0.5 * (mix_end - initial) * output * output / hold
            } else {
                output * mix_end
            }
        } else if ramp > 0.0 && output < hold + ramp {
            let elapsed = output - hold;
            hold_source + mix_end * elapsed + 0.5 * (1.0 - mix_end) * elapsed * elapsed / ramp
        } else {
            hold_source + ramp * (mix_end + 1.0) * 0.5 + (output - hold - ramp)
        };
        Duration::from_secs_f64(source.max(0.0))
    }

    fn hold_source_elapsed(self) -> f64 {
        if self.phase_segment_count > 0 {
            self.segmented_source_elapsed(self.hold).as_secs_f64()
        } else {
            self.hold.as_secs_f64() * f64::from(self.initial_speed + self.mix_end_speed) * 0.5
        }
    }

    fn segmented_source_elapsed(self, output_elapsed: Duration) -> Duration {
        let target = output_elapsed.min(self.hold);
        let mut source = 0.0_f64;
        let mut previous_end = Duration::ZERO;
        for segment in &self.phase_segments[..usize::from(self.phase_segment_count)] {
            let segment_end = segment.output_end.min(target);
            if segment_end > previous_end {
                source += segment_end.saturating_sub(previous_end).as_secs_f64()
                    * f64::from(segment.speed);
            }
            previous_end = segment.output_end;
            if segment.output_end >= target {
                return Duration::from_secs_f64(source);
            }
        }
        if target > previous_end {
            source +=
                target.saturating_sub(previous_end).as_secs_f64() * f64::from(self.mix_end_speed);
        }
        Duration::from_secs_f64(source)
    }

    pub fn output_elapsed(self, source_elapsed: Duration) -> Duration {
        let target = source_elapsed.as_secs_f64();
        let mut low = 0.0_f64;
        let mut high = (target / f64::from(self.initial_speed).clamp(0.01, 1.0))
            + self.hold.as_secs_f64()
            + self.ramp.as_secs_f64();
        for _ in 0..64 {
            let midpoint = (low + high) * 0.5;
            if self
                .source_elapsed(Duration::from_secs_f64(midpoint))
                .as_secs_f64()
                < target
            {
                low = midpoint;
            } else {
                high = midpoint;
            }
        }
        Duration::from_secs_f64((low + high) * 0.5)
    }
}

/// Playback-relative timing for preparing and starting an adaptive transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransitionTiming {
    /// Start resolving the next track at this position in the outgoing track.
    pub prefetch_after: Duration,
    /// Start the overlap at this position in the outgoing track.
    pub transition_after: Duration,
    pub fade_duration: Duration,
}

/// Chooses a safe overlap from the actual lengths of both tracks.
///
/// At most half of either track is consumed by the fade. Prefetch starts one
/// fade window before the overlap, saturating at the beginning for short tracks.
pub fn plan_transition_timing(
    outgoing_duration: Duration,
    incoming_duration: Duration,
    preferred_fade: Duration,
) -> Option<TransitionTiming> {
    let fade_duration = preferred_fade
        .min(outgoing_duration / 2)
        .min(incoming_duration / 2);
    if fade_duration.is_zero() {
        return None;
    }

    let transition_after = outgoing_duration.saturating_sub(fade_duration);
    Some(TransitionTiming {
        prefetch_after: transition_after.saturating_sub(fade_duration),
        transition_after,
        fade_duration,
    })
}

pub fn plan_transition(
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
    let harmonic_compatibility = harmonic_compatibility(outgoing, incoming);
    let duration = if harmonic_compatibility.is_some_and(|score| score < 0.5) {
        timing.fade_duration.min(Duration::from_secs(4))
    } else {
        timing.fade_duration
    };

    let tempo_curve = compatible_tempo_curve(outgoing, incoming, config);
    let beat_aligned = tempo_curve.is_some();
    let phrase_start = beat_aligned
        .then(|| {
            let target = outgoing.audible_end.saturating_sub(duration);
            (incoming.downbeat_confidence >= 0.25 && incoming.first_downbeat.is_some())
                .then(|| align_to_phrase(outgoing, target, duration))
                .flatten()
        })
        .flatten();
    let outgoing_start = if beat_aligned {
        let target = outgoing.audible_end.saturating_sub(duration);
        phrase_start.unwrap_or_else(|| align_to_beat(target, outgoing))
    } else {
        outgoing.audible_end.saturating_sub(duration)
    };
    let incoming_start = if phrase_start.is_some() {
        snap_to_nearest_beat(
            incoming,
            incoming.first_downbeat.unwrap_or(incoming.audible_start),
        )
        .unwrap_or_else(|| {
            incoming
                .first_downbeat
                .unwrap_or(incoming.audible_start)
                .min(incoming.audible_end)
        })
    } else if beat_aligned {
        incoming
            .beat_markers
            .first()
            .copied()
            .or(incoming.first_beat)
            .unwrap_or(incoming.audible_start)
    } else {
        incoming.audible_start
    };
    let (tempo_start, tempo_end) = tempo_curve.unwrap_or((1.0, 1.0));
    let phase_segments = tempo_curve
        .and_then(|_| {
            phase_follow_segments(
                outgoing,
                incoming,
                outgoing_start,
                incoming_start,
                duration,
                config.max_tempo_adjustment,
            )
        })
        .unwrap_or_default();
    let envelope_start = phase_segments
        .first()
        .map_or(tempo_start, |segment| segment.speed);
    let envelope_end = phase_segments
        .last()
        .map_or(tempo_end, |segment| segment.speed);
    let needs_tempo_dsp = phase_segments
        .iter()
        .any(|segment| (segment.speed - 1.0).abs() > TEMPO_SYNC_DEADBAND)
        || (envelope_start - 1.0).abs() > TEMPO_SYNC_DEADBAND
        || (envelope_end - 1.0).abs() > TEMPO_SYNC_DEADBAND
        || (envelope_end - envelope_start).abs() > TEMPO_SYNC_DEADBAND;
    let tempo_envelope = tempo_curve.and_then(|_| {
        needs_tempo_dsp.then(|| {
            TempoEnvelope::new(
                envelope_start,
                envelope_end,
                duration,
                Duration::from_secs_f32(240.0 / outgoing.bpm.unwrap_or(120.0)),
            )
            .with_phase_segments(&phase_segments)
        })
    });
    TransitionPlan {
        kind: if tempo_curve.is_some() {
            TransitionKind::BeatMatched
        } else {
            TransitionKind::Crossfade
        },
        outgoing_start,
        incoming_start,
        duration,
        incoming_tempo_ratio: envelope_start,
        harmonic_compatibility,
        incoming_gain: recommended_incoming_gain(outgoing, incoming),
        tempo_envelope,
    }
}

fn align_to_phrase(
    analysis: &TrackAnalysis,
    target: Duration,
    search_window: Duration,
) -> Option<Duration> {
    let earliest = target.saturating_sub(search_window);
    analysis
        .phrase_cues()
        .into_iter()
        .filter(|cue| cue.position >= earliest && cue.position <= target)
        // Prefer the strongest (longest) phrase boundary, then the latest one.
        .max_by_key(|cue| (cue.length, cue.position))
        .map(|cue| snap_to_nearest_beat(analysis, cue.position).unwrap_or(cue.position))
}

fn align_to_beat(position: Duration, analysis: &TrackAnalysis) -> Duration {
    if let Some(beat) = analysis
        .beat_markers
        .iter()
        .copied()
        .take_while(|beat| *beat <= position)
        .last()
    {
        return beat;
    }
    let first_beat = analysis.first_beat;
    let bpm = analysis.bpm;
    let (Some(first), Some(bpm)) = (first_beat, bpm) else {
        return position;
    };
    if bpm <= 0.0 || position <= first {
        return first.min(position);
    }
    let interval = Duration::from_secs_f32(60.0 / bpm);
    if interval.is_zero() {
        return position;
    }
    let beats = position.saturating_sub(first).as_secs_f64() / interval.as_secs_f64();
    first + interval.mul_f64(beats.floor())
}

fn snap_to_nearest_beat(analysis: &TrackAnalysis, position: Duration) -> Option<Duration> {
    analysis
        .beat_markers
        .iter()
        .copied()
        .min_by_key(|beat| beat.abs_diff(position))
}

fn gapless_plan(outgoing: &TrackAnalysis, incoming: &TrackAnalysis) -> TransitionPlan {
    TransitionPlan {
        kind: TransitionKind::Gapless,
        outgoing_start: outgoing.audible_end,
        incoming_start: incoming.audible_start,
        duration: Duration::ZERO,
        incoming_tempo_ratio: 1.0,
        harmonic_compatibility: harmonic_compatibility(outgoing, incoming),
        incoming_gain: recommended_incoming_gain(outgoing, incoming),
        tempo_envelope: None,
    }
}

fn recommended_incoming_gain(outgoing: &TrackAnalysis, incoming: &TrackAnalysis) -> f32 {
    let Some(outgoing_level) = outgoing.rms_dbfs else {
        return 1.0;
    };
    let Some(incoming_level) = incoming.rms_dbfs else {
        return 1.0;
    };
    let level_gain = 10.0_f32.powf((outgoing_level - incoming_level) / 20.0);
    let peak_gain = incoming
        .sample_peak_dbfs
        .map(|peak| 10.0_f32.powf((-1.0 - peak) / 20.0))
        .unwrap_or(1.0);
    level_gain.min(peak_gain).clamp(0.5, 1.0)
}

pub fn harmonic_compatibility(outgoing: &TrackAnalysis, incoming: &TrackAnalysis) -> Option<f32> {
    let outgoing = outgoing.musical_key?;
    let incoming = incoming.musical_key?;
    if outgoing.confidence < 0.5
        || incoming.confidence < 0.5
        || outgoing.tonic >= 12
        || incoming.tonic >= 12
    {
        return None;
    }
    let score = if outgoing.tonic == incoming.tonic && outgoing.mode == incoming.mode {
        1.0
    } else if matches!(
        (outgoing.mode, incoming.mode),
        (KeyMode::Major, KeyMode::Minor)
    ) && incoming.tonic == (outgoing.tonic + 9) % 12
        || matches!(
            (outgoing.mode, incoming.mode),
            (KeyMode::Minor, KeyMode::Major)
        ) && outgoing.tonic == (incoming.tonic + 9) % 12
    {
        0.95
    } else if outgoing.mode == incoming.mode
        && (incoming.tonic == (outgoing.tonic + 7) % 12
            || outgoing.tonic == (incoming.tonic + 7) % 12)
    {
        0.85
    } else if outgoing.tonic == incoming.tonic {
        0.65
    } else {
        0.2
    };
    Some(score)
}

fn compatible_tempo_curve(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    config: &AutoMixConfig,
) -> Option<(f32, f32)> {
    if outgoing.beat_confidence < config.min_beat_confidence
        || incoming.beat_confidence < config.min_beat_confidence
    {
        return None;
    }
    let outgoing_bpm = outgoing.bpm?;
    let incoming_bpm = incoming.bpm?;
    if outgoing_bpm <= 0.0 || incoming_bpm <= 0.0 {
        return None;
    }

    let ratio = outgoing_bpm / incoming_bpm;
    (ratio.is_finite() && (ratio - 1.0).abs() <= config.max_tempo_adjustment)
        .then_some((ratio, ratio))
}

fn phase_follow_segments(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    outgoing_start: Duration,
    incoming_start: Duration,
    duration: Duration,
    max_tempo_adjustment: f32,
) -> Option<Vec<TempoSegment>> {
    const BEATS_PER_CORRECTION: usize = 4;
    let outgoing_end = outgoing_start.saturating_add(duration);
    let outgoing_beats = outgoing
        .beat_markers
        .iter()
        .copied()
        .filter(|beat| *beat >= outgoing_start && *beat <= outgoing_end)
        .collect::<Vec<_>>();
    let incoming_beats = incoming
        .beat_markers
        .iter()
        .copied()
        .filter(|beat| *beat >= incoming_start)
        .take(outgoing_beats.len())
        .collect::<Vec<_>>();
    let paired = outgoing_beats.len().min(incoming_beats.len());
    if paired < 5
        || outgoing_beats[0].abs_diff(outgoing_start) > Duration::from_millis(30)
        || incoming_beats[0].abs_diff(incoming_start) > Duration::from_millis(30)
    {
        return None;
    }

    let mut segments = Vec::new();
    let mut index = 0;
    while index + 1 < paired && segments.len() < MAX_TEMPO_SEGMENTS {
        let end = (index + BEATS_PER_CORRECTION).min(paired - 1);
        let output_delta = outgoing_beats[end].saturating_sub(outgoing_beats[index]);
        let source_delta = incoming_beats[end].saturating_sub(incoming_beats[index]);
        if output_delta.is_zero() || source_delta.is_zero() {
            return None;
        }
        let speed = source_delta.as_secs_f32() / output_delta.as_secs_f32();
        if !speed.is_finite() || (speed - 1.0).abs() > max_tempo_adjustment {
            return None;
        }
        segments.push(TempoSegment {
            output_end: outgoing_beats[end].saturating_sub(outgoing_start),
            speed,
        });
        index = end;
    }
    (!segments.is_empty()).then_some(segments)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> AutoMixConfig {
        AutoMixConfig {
            enabled: true,
            crossfade: Duration::from_secs(8),
            max_tempo_adjustment: 0.06,
            min_beat_confidence: 0.7,
        }
    }

    fn analyzed(bpm: f32) -> TrackAnalysis {
        let interval = Duration::from_secs_f32(60.0 / bpm);
        let mut beat_markers = Vec::new();
        let mut beat = Duration::from_secs(1);
        while beat <= Duration::from_secs(179) {
            beat_markers.push(beat);
            beat += interval;
        }
        TrackAnalysis {
            duration: Duration::from_secs(180),
            audible_start: Duration::from_secs(1),
            audible_end: Duration::from_secs(179),
            bpm: Some(bpm),
            beat_confidence: 0.9,
            first_beat: Some(Duration::from_secs(1)),
            beat_markers,
            first_downbeat: Some(Duration::from_secs(1)),
            downbeat_confidence: 0.9,
            musical_key: None,
            rms_dbfs: None,
            sample_peak_dbfs: None,
        }
    }

    fn keyed(tonic: u8, mode: KeyMode) -> TrackAnalysis {
        let mut analysis = analyzed(120.0);
        analysis.musical_key = Some(MusicalKey {
            tonic,
            mode,
            confidence: 0.9,
        });
        analysis
    }

    #[test]
    fn exposes_four_four_grid_and_hierarchical_phrase_cues() {
        let analysis = analyzed(120.0);
        let grid = analysis.beat_grid().unwrap();
        assert_eq!(grid.first_downbeat, Duration::from_secs(1));
        assert_eq!(grid.beat_interval, Duration::from_millis(500));
        assert_eq!(grid.beats_per_bar, 4);
        assert_eq!(grid.downbeat_confidence, 0.9);

        let cues = analysis.phrase_cues();
        assert!(cues.contains(&PhraseCue {
            position: Duration::from_secs(9),
            length: PhraseLength::FourBars,
        }));
        assert!(cues.contains(&PhraseCue {
            position: Duration::from_secs(17),
            length: PhraseLength::EightBars,
        }));
        assert!(cues.contains(&PhraseCue {
            position: Duration::from_secs(33),
            length: PhraseLength::SixteenBars,
        }));
    }

    #[test]
    fn beatmatch_prefers_phrase_boundary_near_the_fade_target() {
        let plan = plan_transition(&analyzed(120.0), &analyzed(120.0), &config());
        // Fade target is 171s. The inferred 4-bar boundary at 169s is the
        // strongest boundary within the eight-second look-behind window.
        assert_eq!(plan.outgoing_start, Duration::from_secs(169));
    }

    #[test]
    fn chooses_beatmatch_for_compatible_confident_tempos() {
        let plan = plan_transition(&analyzed(120.0), &analyzed(124.0), &config());
        assert_eq!(plan.kind, TransitionKind::BeatMatched);
        assert!((plan.incoming_tempo_ratio - 120.0 / 124.0).abs() < 0.0001);
        assert_eq!(plan.duration, Duration::from_secs(8));
    }

    #[test]
    fn corrects_small_tempo_differences_that_would_drift_during_the_mix() {
        let plan = plan_transition(&analyzed(120.0), &analyzed(120.3), &config());
        let envelope = plan
            .tempo_envelope
            .expect("small drift should be corrected");
        assert!((envelope.initial_speed - 120.0 / 120.3).abs() < 0.0001);
        assert_eq!(envelope.hold, plan.duration);
    }

    #[test]
    fn falls_back_to_crossfade_for_incompatible_tempos() {
        let plan = plan_transition(&analyzed(90.0), &analyzed(140.0), &config());
        assert_eq!(plan.kind, TransitionKind::Crossfade);
        assert_eq!(plan.incoming_tempo_ratio, 1.0);
    }

    #[test]
    fn recognizes_relative_keys_as_harmonically_compatible() {
        let plan = plan_transition(
            &keyed(0, KeyMode::Major),
            &keyed(9, KeyMode::Minor),
            &config(),
        );
        assert_eq!(plan.harmonic_compatibility, Some(0.95));
        assert_eq!(plan.duration, Duration::from_secs(8));
    }

    #[test]
    fn shortens_overlap_for_confident_incompatible_keys() {
        let plan = plan_transition(
            &keyed(0, KeyMode::Major),
            &keyed(6, KeyMode::Major),
            &config(),
        );
        assert_eq!(plan.harmonic_compatibility, Some(0.2));
        assert_eq!(plan.duration, Duration::from_secs(4));
    }

    #[test]
    fn attenuates_a_louder_incoming_track_without_boosting() {
        let mut outgoing = analyzed(120.0);
        outgoing.rms_dbfs = Some(-18.0);
        let mut incoming = analyzed(120.0);
        incoming.rms_dbfs = Some(-12.0);
        incoming.sample_peak_dbfs = Some(-0.5);
        let plan = plan_transition(&outgoing, &incoming, &config());
        assert!((plan.incoming_gain - 0.501).abs() < 0.01);

        std::mem::swap(&mut outgoing, &mut incoming);
        let plan = plan_transition(&outgoing, &incoming, &config());
        assert_eq!(plan.incoming_gain, 1.0);
    }

    #[test]
    fn tempo_envelope_holds_then_returns_to_native_speed() {
        let envelope =
            TempoEnvelope::new(0.95, 0.95, Duration::from_secs(8), Duration::from_secs(2));
        assert_eq!(envelope.speed_at(Duration::from_secs(4)), 0.95);
        assert!((envelope.speed_at(Duration::from_secs(9)) - 0.975).abs() < 0.0001);
        assert_eq!(envelope.speed_at(Duration::from_secs(11)), 1.0);
        assert!(
            (envelope
                .source_elapsed(Duration::from_secs(10))
                .as_secs_f32()
                - 9.55)
                .abs()
                < 0.001
        );
    }

    #[test]
    fn tempo_envelope_time_mapping_round_trips() {
        let envelope =
            TempoEnvelope::new(1.04, 1.04, Duration::from_secs(8), Duration::from_secs(2));
        for output in [0.0, 4.0, 8.0, 9.0, 12.0, 60.0] {
            let output = Duration::from_secs_f64(output);
            let source = envelope.source_elapsed(output);
            assert!(envelope.output_elapsed(source).abs_diff(output) < Duration::from_micros(2));
        }
    }

    #[test]
    fn tempo_envelope_follows_local_tempo_during_overlap() {
        let envelope =
            TempoEnvelope::new(0.98, 1.02, Duration::from_secs(8), Duration::from_secs(2));

        assert!((envelope.speed_at(Duration::ZERO) - 0.98).abs() < 0.0001);
        assert!((envelope.speed_at(Duration::from_secs(4)) - 1.0).abs() < 0.0001);
        assert!((envelope.speed_at(Duration::from_secs(8)) - 1.02).abs() < 0.0001);
    }

    #[test]
    fn transition_tracks_tempo_drift_across_the_overlap() {
        let mut outgoing = analyzed(120.0);
        outgoing
            .beat_markers
            .retain(|beat| *beat <= Duration::from_secs(170));
        let mut beat = *outgoing.beat_markers.last().unwrap();
        for index in 0..16 {
            let bpm = if index < 8 { 120.0 } else { 123.0 };
            beat += Duration::from_secs_f32(60.0 / bpm);
            outgoing.beat_markers.push(beat);
        }
        let incoming = analyzed(120.0);

        let plan = plan_transition(&outgoing, &incoming, &config());
        let envelope = plan.tempo_envelope.expect("tempo drift should be followed");

        assert!((envelope.initial_speed - 1.0).abs() < 0.001);
        assert!((envelope.mix_end_speed - (123.0 / 120.0)).abs() < 0.001);
        assert!(envelope.speed_at(plan.duration) > envelope.speed_at(Duration::ZERO));

        let outgoing_beats = outgoing
            .beat_markers
            .iter()
            .copied()
            .filter(|beat| *beat >= plan.outgoing_start)
            .collect::<Vec<_>>();
        let incoming_beats = incoming
            .beat_markers
            .iter()
            .copied()
            .filter(|beat| *beat >= plan.incoming_start)
            .collect::<Vec<_>>();
        for segment in &envelope.phase_segments[..usize::from(envelope.phase_segment_count)] {
            let outgoing_beat = plan.outgoing_start + segment.output_end;
            let index = outgoing_beats
                .iter()
                .position(|beat| beat.abs_diff(outgoing_beat) < Duration::from_millis(1))
                .unwrap();
            let expected_source = incoming_beats[index].saturating_sub(plan.incoming_start);
            assert!(
                envelope
                    .source_elapsed(segment.output_end)
                    .abs_diff(expected_source)
                    < Duration::from_millis(1)
            );
        }
    }

    #[test]
    fn disabled_automix_preserves_trimmed_gapless_boundary() {
        let mut config = config();
        config.enabled = false;
        let outgoing = analyzed(120.0);
        let incoming = analyzed(120.0);
        let plan = plan_transition(&outgoing, &incoming, &config);
        assert_eq!(plan.kind, TransitionKind::Gapless);
        assert_eq!(plan.outgoing_start, outgoing.audible_end);
        assert_eq!(plan.incoming_start, incoming.audible_start);
    }

    #[test]
    fn adaptive_timing_uses_preferred_fade_for_long_tracks() {
        let timing = plan_transition_timing(
            Duration::from_secs(180),
            Duration::from_secs(240),
            Duration::from_secs(8),
        )
        .unwrap();

        assert_eq!(timing.fade_duration, Duration::from_secs(8));
        assert_eq!(timing.transition_after, Duration::from_secs(172));
        assert_eq!(timing.prefetch_after, Duration::from_secs(164));
    }

    #[test]
    fn adaptive_timing_is_bounded_by_the_shorter_track() {
        let timing = plan_transition_timing(
            Duration::from_secs(12),
            Duration::from_secs(6),
            Duration::from_secs(8),
        )
        .unwrap();

        assert_eq!(timing.fade_duration, Duration::from_secs(3));
        assert_eq!(timing.transition_after, Duration::from_secs(9));
        assert_eq!(timing.prefetch_after, Duration::from_secs(6));
    }

    #[test]
    fn adaptive_timing_rejects_zero_length_boundaries() {
        assert_eq!(
            plan_transition_timing(
                Duration::ZERO,
                Duration::from_secs(60),
                Duration::from_secs(8)
            ),
            None
        );
        assert_eq!(
            plan_transition_timing(
                Duration::from_secs(60),
                Duration::from_secs(60),
                Duration::ZERO
            ),
            None
        );
    }
}
