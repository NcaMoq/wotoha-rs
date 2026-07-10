use std::time::Duration;

use crate::vocal_analysis::effective_vocal_risk;

pub const TEMPO_SYNC_DEADBAND: f32 = 0.001;
const MAX_TEMPO_SEGMENTS: usize = 32;
const MIN_PHASE_MARKER_CONFIDENCE: f32 = 0.35;
const MIN_AUDIBLE_MIX_OVERLAP: Duration = Duration::from_secs(1);
const MAX_BEATMATCH_PHASE_ERROR: Duration = Duration::from_millis(35);
const MAX_DOWNBEAT_PHASE_ERROR: Duration = Duration::from_millis(70);
const MAX_PHRASE_PHASE_ERROR: Duration = Duration::from_millis(150);
const MIN_LOW_HANDOFF_GAIN: f32 = 0.85;
const MAX_LOW_HANDOFF_GAIN: f32 = 1.15;
const MAX_DUAL_VOCAL_RISK: f32 = 0.58;
const ENERGY_SELECTION_EPSILON: f32 = 0.02;
const MAX_ENERGY_START_CANDIDATES: usize = 96;
const MIN_MARKER_BACKED_BEAT_CONFIDENCE: f32 = 0.45;
const MIN_MARKER_BACKED_KICK_COVERAGE: f32 = 0.55;
const MIN_MARKER_BACKED_KICK_MARKERS: usize = 8;
const MIN_ENERGY_PROFILE_DBFS: f32 = -80.0;
const MIX_PEAK_HEADROOM_DBFS: f32 = -1.0;

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
    pub intro_end: Option<Duration>,
    pub intro_confidence: f32,
    pub outro_start: Option<Duration>,
    pub outro_confidence: f32,
    /// Quantized vocal activity probability at `vocal_activity_rate` Hz.
    pub vocal_activity: Vec<u8>,
    /// Quantized confidence for each vocal activity sample.
    pub vocal_activity_confidences: Vec<u8>,
    pub vocal_activity_rate: u8,
    /// Quantized full-band RMS profile used to score transition energy.
    pub energy_profile: Vec<u8>,
    pub energy_profile_rate: u8,
    pub bpm: Option<f32>,
    pub beat_confidence: f32,
    pub first_beat: Option<Duration>,
    /// Detected beat onsets used to follow local tempo changes during a transition.
    pub beat_markers: Vec<Duration>,
    /// Per-marker confidence that the onset is a low-frequency kick.
    pub beat_marker_confidences: Vec<f32>,
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
            intro_end: None,
            intro_confidence: 0.0,
            outro_start: None,
            outro_confidence: 0.0,
            vocal_activity: Vec::new(),
            vocal_activity_confidences: Vec::new(),
            vocal_activity_rate: 0,
            energy_profile: Vec::new(),
            energy_profile_rate: 0,
            bpm: None,
            beat_confidence: 0.0,
            first_beat: None,
            beat_markers: Vec::new(),
            beat_marker_confidences: Vec::new(),
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

    pub fn trusted_kick_coverage(&self) -> f32 {
        if self.beat_marker_confidences.is_empty() {
            return 0.0;
        }
        self.beat_marker_confidences
            .iter()
            .filter(|confidence| **confidence >= MIN_PHASE_MARKER_CONFIDENCE)
            .count() as f32
            / self.beat_marker_confidences.len() as f32
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

/// Identifies which deck an equalizer curve belongs to during an AutoMix overlap.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EqTransitionRole {
    Outgoing,
    Incoming,
}

/// Per-band linear gain. A value of `1.0` leaves the band unchanged.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EqGains {
    pub low: f32,
    pub mid: f32,
    pub high: f32,
}

/// A source-timeline EQ automation used to exchange bass between overlapping decks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EqTransition {
    /// Stable identifier used to replace or cancel scheduled automation.
    pub id: u64,
    pub source_start: Duration,
    pub duration: Duration,
    pub role: EqTransitionRole,
}

impl EqTransition {
    /// Returns the equalizer gains at an absolute position on this track's source timeline.
    ///
    /// The incoming deck starts with its low band removed and restores it by the
    /// midpoint. The outgoing deck performs the complementary bass handoff.
    /// The incoming mid/high bands open during the first half. The outgoing
    /// mid/high bands remain clear through the bass handoff, then recede during
    /// the second half while the regular crossfade owns the overall level.
    pub fn gains_at(self, timeline_position: Duration) -> EqGains {
        let progress = if timeline_position < self.source_start {
            0.0
        } else if self.duration.is_zero() {
            1.0
        } else {
            timeline_position
                .saturating_sub(self.source_start)
                .as_secs_f32()
                / self.duration.as_secs_f32()
        }
        .clamp(0.0, 1.0);
        let bass_handoff = smoothstep((progress * 2.0).clamp(0.0, 1.0));
        let outgoing_recede = smoothstep(((progress - 0.5) * 2.0).clamp(0.0, 1.0));
        let (low, mid, high) = match self.role {
            EqTransitionRole::Outgoing => (
                1.0 - bass_handoff,
                1.0 - 0.3 * outgoing_recede,
                1.0 - 0.2 * outgoing_recede,
            ),
            EqTransitionRole::Incoming => (
                bass_handoff,
                0.65 + 0.35 * bass_handoff,
                0.8 + 0.2 * bass_handoff,
            ),
        };

        EqGains {
            low: low.clamp(0.0, 1.0),
            mid: mid.clamp(0.0, 1.0),
            high: high.clamp(0.0, 1.0),
        }
    }
}

fn smoothstep(value: f32) -> f32 {
    let value = value.clamp(0.0, 1.0);
    value * value * (3.0 - 2.0 * value)
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
    pub energy_selection: Option<AutoMixEnergySelection>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AutoMixEnergySelection {
    pub default_start: Duration,
    pub selected_start: Duration,
    pub candidates_checked: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AutoMixQualityReport {
    pub issues: Vec<AutoMixQualityIssue>,
    pub overlap: Duration,
    pub beat_pairs_checked: usize,
    pub max_beat_phase_error: Option<Duration>,
    pub handoff_beat_phase_error: Option<Duration>,
    pub downbeat_pairs_checked: usize,
    pub max_downbeat_phase_error: Option<Duration>,
    pub handoff_downbeat_phase_error: Option<Duration>,
    pub phrase_pairs_checked: usize,
    pub max_phrase_phase_error: Option<Duration>,
    pub handoff_phrase_phase_error: Option<Duration>,
    pub low_handoff_min: Option<f32>,
    pub low_handoff_max: Option<f32>,
    pub vocal_overlap_samples_checked: usize,
    pub max_dual_vocal_risk: Option<f32>,
    pub energy_samples_checked: usize,
    pub min_mix_energy_ratio: Option<f32>,
    pub max_mix_energy_ratio: Option<f32>,
}

impl AutoMixQualityReport {
    pub fn is_ok(&self) -> bool {
        self.issues.is_empty()
    }

    pub fn has_blocking_issue(&self) -> bool {
        self.issues
            .iter()
            .any(AutoMixQualityIssue::blocks_automatic_transition)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AutoMixQualityIssue {
    MixOverlapTooShort {
        overlap: Duration,
    },
    OutgoingOverlapMissesAudibleEnd {
        actual: Duration,
        expected: Duration,
    },
    IncomingOverlapExceedsAudibleEnd {
        actual: Duration,
        expected: Duration,
    },
    BeatPhaseUnverified,
    BeatPhaseDriftTooLarge {
        max_error: Duration,
    },
    BeatHandoffPhaseDriftTooLarge {
        error: Duration,
    },
    DownbeatPhaseDriftTooLarge {
        max_error: Duration,
    },
    DownbeatHandoffPhaseDriftTooLarge {
        error: Duration,
    },
    PhrasePhaseDriftTooLarge {
        max_error: Duration,
    },
    PhraseHandoffPhaseDriftTooLarge {
        error: Duration,
    },
    LowHandoffDip {
        min_gain: f32,
    },
    LowHandoffBuildUp {
        max_gain: f32,
    },
    DualVocalOverlapTooHigh {
        max_risk: f32,
    },
}

impl AutoMixQualityIssue {
    pub fn blocks_automatic_transition(&self) -> bool {
        matches!(
            self,
            Self::MixOverlapTooShort { .. }
                | Self::OutgoingOverlapMissesAudibleEnd { .. }
                | Self::IncomingOverlapExceedsAudibleEnd { .. }
                | Self::BeatPhaseUnverified
                | Self::BeatPhaseDriftTooLarge { .. }
                | Self::BeatHandoffPhaseDriftTooLarge { .. }
                | Self::DownbeatPhaseDriftTooLarge { .. }
                | Self::DownbeatHandoffPhaseDriftTooLarge { .. }
                | Self::PhrasePhaseDriftTooLarge { .. }
                | Self::PhraseHandoffPhaseDriftTooLarge { .. }
                | Self::DualVocalOverlapTooHigh { .. }
        )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct GuardedTransitionPlan {
    pub plan: TransitionPlan,
    pub quality: AutoMixQualityReport,
    pub rejected_plan: Option<TransitionPlan>,
    pub rejected_quality: Option<AutoMixQualityReport>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AutoMixBeatMatchDecision {
    Selected,
    Disabled,
    QualityGuarded,
    NoTrustedIncomingBeatStart,
    OutgoingTempoConfidenceTooLow,
    IncomingTempoConfidenceTooLow,
    MissingBpm,
    InvalidBpm,
    TempoDifferenceTooLarge,
    VocalLimitConstrained,
    NoSafeOverlap,
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
        if self.phase_segment_count == 0
            && (self.initial_speed - 1.0).abs() <= f32::EPSILON
            && (self.mix_end_speed - 1.0).abs() <= f32::EPSILON
        {
            return source_elapsed;
        }
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
    let incoming_gain = recommended_incoming_gain(outgoing, incoming);
    let base_duration = if harmonic_compatibility.is_some_and(|score| score < 0.5) {
        timing.fade_duration.min(Duration::from_secs(4))
    } else {
        timing.fade_duration
    };
    let structured_duration = trusted_structure_overlap(outgoing, incoming, base_duration);
    let incoming_beat_start = safe_incoming_beat_start(incoming);
    let tempo_curve =
        incoming_beat_start.and_then(|_| compatible_tempo_curve(outgoing, incoming, config));
    let beat_aligned = tempo_curve.is_some();
    let mut use_beatmatch = beat_aligned;
    let incoming_start = if beat_aligned {
        incoming_beat_start.unwrap_or(incoming.audible_start)
    } else {
        incoming.audible_start
    };
    let mut target_duration = structured_duration.unwrap_or(base_duration);
    let preliminary_envelope = tempo_curve
        .map(|(start, end)| TempoEnvelope::new(start, end, target_duration, Duration::ZERO));
    let preliminary_vocal_limit = vocal_overlap_limit(
        outgoing,
        incoming,
        incoming_start,
        target_duration,
        preliminary_envelope,
    );
    if preliminary_vocal_limit.is_zero() {
        return gapless_plan(outgoing, incoming);
    }
    let vocal_constrained = preliminary_vocal_limit < target_duration;
    target_duration = target_duration.min(preliminary_vocal_limit);
    let structure_adaptive = structured_duration.is_some();
    let phase_bias = if vocal_constrained || structure_adaptive {
        BarPhaseBias::AtOrAfter
    } else {
        BarPhaseBias::AtOrBefore
    };
    let phrase_start = beat_aligned
        .then(|| {
            let target = outgoing.audible_end.saturating_sub(target_duration);
            (harmonic_compatibility.is_none_or(|score| score >= 0.5)
                && incoming.downbeat_confidence >= 0.25
                && incoming.first_downbeat.is_some())
            .then(|| {
                align_to_matching_phrase_phase(
                    outgoing,
                    incoming,
                    incoming_start,
                    target,
                    target_duration,
                    PhraseLength::FourBars,
                    phase_bias,
                )
                .or_else(|| {
                    matches!(phase_bias, BarPhaseBias::AtOrBefore)
                        .then(|| align_to_phrase(outgoing, target, target_duration))
                        .flatten()
                })
            })
            .flatten()
        })
        .flatten();
    let bar_phase_start = beat_aligned
        .then(|| {
            let target = outgoing.audible_end.saturating_sub(target_duration);
            align_to_matching_bar_phase(outgoing, incoming, incoming_start, target, phase_bias)
        })
        .flatten();
    let target = outgoing.audible_end.saturating_sub(target_duration);
    let mut outgoing_start = if beat_aligned {
        if vocal_constrained {
            phrase_start
                .or(bar_phase_start)
                .unwrap_or_else(|| align_to_beat_at_or_after(target, outgoing))
        } else if structure_adaptive {
            phrase_start
                .or(bar_phase_start)
                .or_else(|| snap_to_nearest_beat(outgoing, target))
                .unwrap_or(target)
        } else {
            phrase_start
                .or(bar_phase_start)
                .unwrap_or_else(|| align_to_beat(target, outgoing))
        }
    } else {
        outgoing.audible_end.saturating_sub(target_duration)
    };
    let (selected_start, mut energy_selection) = select_energy_balanced_start(
        outgoing,
        incoming,
        incoming_start,
        outgoing_start,
        target,
        target_duration,
        beat_aligned,
        phase_bias,
        harmonic_compatibility,
        incoming_gain,
        tempo_curve,
        config.max_tempo_adjustment,
    );
    outgoing_start = selected_start;
    let mut duration = outgoing.audible_end.saturating_sub(outgoing_start);
    let (mut envelope_start, mut tempo_envelope) = build_tempo_envelope(
        outgoing,
        incoming,
        outgoing_start,
        incoming_start,
        duration,
        tempo_curve,
        config.max_tempo_adjustment,
    );
    let exact_vocal_limit =
        vocal_overlap_limit(outgoing, incoming, incoming_start, duration, tempo_envelope);
    if exact_vocal_limit.is_zero() {
        return gapless_plan(outgoing, incoming);
    }
    if exact_vocal_limit < duration {
        let target = outgoing.audible_end.saturating_sub(exact_vocal_limit);
        outgoing_start = if beat_aligned {
            align_to_matching_phrase_phase(
                outgoing,
                incoming,
                incoming_start,
                target,
                exact_vocal_limit,
                PhraseLength::FourBars,
                BarPhaseBias::AtOrAfter,
            )
            .or_else(|| {
                align_to_matching_bar_phase(
                    outgoing,
                    incoming,
                    incoming_start,
                    target,
                    BarPhaseBias::AtOrAfter,
                )
            })
            .unwrap_or_else(|| align_to_beat_at_or_after(target, outgoing))
        } else {
            target
        };
        energy_selection = None;
        duration = outgoing.audible_end.saturating_sub(outgoing_start);
        (envelope_start, tempo_envelope) = build_tempo_envelope(
            outgoing,
            incoming,
            outgoing_start,
            incoming_start,
            duration,
            tempo_curve,
            config.max_tempo_adjustment,
        );
    }
    let final_vocal_limit =
        vocal_overlap_limit(outgoing, incoming, incoming_start, duration, tempo_envelope);
    if final_vocal_limit.is_zero() {
        return gapless_plan(outgoing, incoming);
    }
    if final_vocal_limit < duration {
        envelope_start = 1.0;
        tempo_envelope = None;
        use_beatmatch = false;
        // The previous limit was measured through the tempo map. Once tempo
        // matching is abandoned, validate against native incoming time again.
        let native_vocal_limit =
            vocal_overlap_limit(outgoing, incoming, incoming_start, duration, None);
        if native_vocal_limit.is_zero() {
            return gapless_plan(outgoing, incoming);
        }
        outgoing_start = outgoing.audible_end.saturating_sub(native_vocal_limit);
        energy_selection = None;
        duration = native_vocal_limit;
    }
    TransitionPlan {
        kind: if use_beatmatch {
            TransitionKind::BeatMatched
        } else {
            TransitionKind::Crossfade
        },
        outgoing_start,
        incoming_start,
        duration,
        incoming_tempo_ratio: envelope_start,
        harmonic_compatibility,
        incoming_gain,
        tempo_envelope,
        energy_selection,
    }
}

pub fn plan_guarded_transition(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    config: &AutoMixConfig,
) -> GuardedTransitionPlan {
    let plan = plan_transition(outgoing, incoming, config);
    let quality = evaluate_transition_quality(outgoing, incoming, &plan);
    if !quality.has_blocking_issue() {
        return GuardedTransitionPlan {
            plan,
            quality,
            rejected_plan: None,
            rejected_quality: None,
        };
    }

    let fallback = conservative_crossfade_plan(outgoing, incoming, config);
    let fallback_quality = evaluate_transition_quality(outgoing, incoming, &fallback);
    GuardedTransitionPlan {
        plan: fallback,
        quality: fallback_quality,
        rejected_plan: Some(plan),
        rejected_quality: Some(quality),
    }
}

pub fn explain_beatmatch_decision(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    config: &AutoMixConfig,
    guarded: &GuardedTransitionPlan,
) -> AutoMixBeatMatchDecision {
    if !config.enabled {
        return AutoMixBeatMatchDecision::Disabled;
    }
    if guarded.plan.kind == TransitionKind::BeatMatched {
        return AutoMixBeatMatchDecision::Selected;
    }
    if guarded
        .rejected_plan
        .as_ref()
        .is_some_and(|plan| plan.kind == TransitionKind::BeatMatched)
    {
        return AutoMixBeatMatchDecision::QualityGuarded;
    }
    if safe_incoming_beat_start(incoming).is_none() {
        return AutoMixBeatMatchDecision::NoTrustedIncomingBeatStart;
    }
    if !tempo_alignment_confident(outgoing, config.min_beat_confidence) {
        return AutoMixBeatMatchDecision::OutgoingTempoConfidenceTooLow;
    }
    if !tempo_alignment_confident(incoming, config.min_beat_confidence) {
        return AutoMixBeatMatchDecision::IncomingTempoConfidenceTooLow;
    }

    let (Some(outgoing_bpm), Some(incoming_bpm)) = (outgoing.bpm, incoming.bpm) else {
        return AutoMixBeatMatchDecision::MissingBpm;
    };
    if outgoing_bpm <= 0.0 || incoming_bpm <= 0.0 {
        return AutoMixBeatMatchDecision::InvalidBpm;
    }
    let Some(ratio) = closest_tempo_family_ratio(outgoing_bpm, incoming_bpm) else {
        return AutoMixBeatMatchDecision::InvalidBpm;
    };
    if !ratio.is_finite() || (ratio - 1.0).abs() > config.max_tempo_adjustment {
        return AutoMixBeatMatchDecision::TempoDifferenceTooLarge;
    }

    match guarded.plan.kind {
        TransitionKind::Gapless => AutoMixBeatMatchDecision::NoSafeOverlap,
        TransitionKind::Crossfade => AutoMixBeatMatchDecision::VocalLimitConstrained,
        TransitionKind::BeatMatched => AutoMixBeatMatchDecision::Selected,
    }
}

pub fn evaluate_transition_quality(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    plan: &TransitionPlan,
) -> AutoMixQualityReport {
    let mut issues = Vec::new();
    let mut beat_pairs_checked = 0;
    let mut max_beat_phase_error = None;
    let mut handoff_beat_phase_error = None;
    let mut downbeat_pairs_checked = 0;
    let mut max_downbeat_phase_error = None;
    let mut handoff_downbeat_phase_error = None;
    let mut phrase_pairs_checked = 0;
    let mut max_phrase_phase_error = None;
    let mut handoff_phrase_phase_error = None;
    let mut vocal_overlap_samples_checked = 0;
    let mut max_dual_vocal_risk = None;
    let mut energy_samples_checked = 0;
    let mut min_mix_energy_ratio = None;
    let mut max_mix_energy_ratio = None;
    let (low_handoff_min, low_handoff_max) = low_handoff_range(plan);

    if plan.kind != TransitionKind::Gapless {
        if plan.duration < MIN_AUDIBLE_MIX_OVERLAP {
            issues.push(AutoMixQualityIssue::MixOverlapTooShort {
                overlap: plan.duration,
            });
        }

        let actual_outgoing_end = plan.outgoing_start.saturating_add(plan.duration);
        if actual_outgoing_end.abs_diff(outgoing.audible_end) > Duration::from_millis(20) {
            issues.push(AutoMixQualityIssue::OutgoingOverlapMissesAudibleEnd {
                actual: actual_outgoing_end,
                expected: outgoing.audible_end,
            });
        }

        let incoming_overlap_end = plan
            .incoming_start
            .saturating_add(incoming_mix_source(plan));
        if incoming_overlap_end
            > incoming
                .audible_end
                .saturating_add(Duration::from_millis(20))
        {
            issues.push(AutoMixQualityIssue::IncomingOverlapExceedsAudibleEnd {
                actual: incoming_overlap_end,
                expected: incoming.audible_end,
            });
        }

        if let (Some(min_gain), Some(max_gain)) = (low_handoff_min, low_handoff_max) {
            if min_gain < MIN_LOW_HANDOFF_GAIN {
                issues.push(AutoMixQualityIssue::LowHandoffDip { min_gain });
            }
            if max_gain > MAX_LOW_HANDOFF_GAIN {
                issues.push(AutoMixQualityIssue::LowHandoffBuildUp { max_gain });
            }
        }

        let vocal_overlap = dual_vocal_overlap_report(outgoing, incoming, plan);
        vocal_overlap_samples_checked = vocal_overlap.samples;
        max_dual_vocal_risk = vocal_overlap.max_risk;
        if let Some(max_risk) = max_dual_vocal_risk
            && max_risk > MAX_DUAL_VOCAL_RISK
        {
            issues.push(AutoMixQualityIssue::DualVocalOverlapTooHigh { max_risk });
        }

        let energy = transition_energy_report(outgoing, incoming, plan);
        energy_samples_checked = energy.samples;
        min_mix_energy_ratio = energy.min_ratio;
        max_mix_energy_ratio = energy.max_ratio;
    }

    if plan.kind == TransitionKind::BeatMatched {
        let phase = beat_phase_report(outgoing, incoming, plan);
        beat_pairs_checked = phase.pairs;
        max_beat_phase_error = phase.max_error;
        match phase.max_error {
            Some(max_error) if max_error > MAX_BEATMATCH_PHASE_ERROR => {
                issues.push(AutoMixQualityIssue::BeatPhaseDriftTooLarge { max_error });
            }
            Some(_) => {}
            None => issues.push(AutoMixQualityIssue::BeatPhaseUnverified),
        }
        handoff_beat_phase_error = beat_handoff_phase_error(outgoing, incoming, plan);
        if let Some(error) = handoff_beat_phase_error
            && error > MAX_BEATMATCH_PHASE_ERROR
        {
            issues.push(AutoMixQualityIssue::BeatHandoffPhaseDriftTooLarge { error });
        }

        let downbeat_phase = downbeat_phase_report(outgoing, incoming, plan);
        downbeat_pairs_checked = downbeat_phase.pairs;
        max_downbeat_phase_error = downbeat_phase.max_error;
        if let Some(max_error) = downbeat_phase.max_error
            && max_error > MAX_DOWNBEAT_PHASE_ERROR
        {
            issues.push(AutoMixQualityIssue::DownbeatPhaseDriftTooLarge { max_error });
        }
        handoff_downbeat_phase_error = downbeat_handoff_phase_error(outgoing, incoming, plan);
        if let Some(error) = handoff_downbeat_phase_error
            && error > MAX_DOWNBEAT_PHASE_ERROR
        {
            issues.push(AutoMixQualityIssue::DownbeatHandoffPhaseDriftTooLarge { error });
        }

        let phrase_phase = phrase_phase_report(outgoing, incoming, plan, PhraseLength::FourBars);
        phrase_pairs_checked = phrase_phase.pairs;
        max_phrase_phase_error = phrase_phase.max_error;
        if let Some(max_error) = phrase_phase.max_error
            && max_error > MAX_PHRASE_PHASE_ERROR
            && phrase_phase_is_actionable(outgoing, incoming, plan, PhraseLength::FourBars)
        {
            issues.push(AutoMixQualityIssue::PhrasePhaseDriftTooLarge { max_error });
        }
        handoff_phrase_phase_error =
            phrase_handoff_phase_error(outgoing, incoming, plan, PhraseLength::FourBars);
        if let Some(error) = handoff_phrase_phase_error
            && error > MAX_PHRASE_PHASE_ERROR
            && phrase_phase_is_actionable(outgoing, incoming, plan, PhraseLength::FourBars)
        {
            issues.push(AutoMixQualityIssue::PhraseHandoffPhaseDriftTooLarge { error });
        }
    }

    AutoMixQualityReport {
        issues,
        overlap: plan.duration,
        beat_pairs_checked,
        max_beat_phase_error,
        handoff_beat_phase_error,
        downbeat_pairs_checked,
        max_downbeat_phase_error,
        handoff_downbeat_phase_error,
        phrase_pairs_checked,
        max_phrase_phase_error,
        handoff_phrase_phase_error,
        low_handoff_min,
        low_handoff_max,
        vocal_overlap_samples_checked,
        max_dual_vocal_risk,
        energy_samples_checked,
        min_mix_energy_ratio,
        max_mix_energy_ratio,
    }
}

#[derive(Clone, Copy)]
struct BeatPhaseReport {
    pairs: usize,
    max_error: Option<Duration>,
}

fn beat_phase_report(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    plan: &TransitionPlan,
) -> BeatPhaseReport {
    let outgoing_end = plan.outgoing_start.saturating_add(plan.duration);
    let incoming_end = plan
        .incoming_start
        .saturating_add(incoming_mix_source(plan));
    let outgoing_beats = beat_positions_between(outgoing, plan.outgoing_start, outgoing_end);
    let incoming_beats = beat_positions_between(incoming, plan.incoming_start, incoming_end);
    let mut pairs = 0;
    let mut max_error = None;

    for outgoing_beat in outgoing_beats {
        let output_elapsed = outgoing_beat.saturating_sub(plan.outgoing_start);
        let incoming_position = plan
            .incoming_start
            .saturating_add(incoming_source_elapsed(plan, output_elapsed));
        let Some(error) = incoming_beats
            .iter()
            .map(|beat| beat.abs_diff(incoming_position))
            .min()
        else {
            continue;
        };
        pairs += 1;
        max_error = Some(max_error.map_or(error, |current: Duration| current.max(error)));
    }

    BeatPhaseReport {
        pairs,
        max_error: (pairs > 0).then_some(max_error).flatten(),
    }
}

fn downbeat_phase_report(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    plan: &TransitionPlan,
) -> BeatPhaseReport {
    let outgoing_end = plan.outgoing_start.saturating_add(plan.duration);
    let incoming_end = plan
        .incoming_start
        .saturating_add(incoming_mix_source(plan));
    let outgoing_downbeats =
        downbeat_positions_between(outgoing, plan.outgoing_start, outgoing_end);
    let incoming_downbeats =
        downbeat_positions_between(incoming, plan.incoming_start, incoming_end);
    let mut pairs = 0;
    let mut max_error = None;

    for outgoing_downbeat in outgoing_downbeats {
        let output_elapsed = outgoing_downbeat.saturating_sub(plan.outgoing_start);
        let incoming_position = plan
            .incoming_start
            .saturating_add(incoming_source_elapsed(plan, output_elapsed));
        let Some(error) = incoming_downbeats
            .iter()
            .map(|downbeat| downbeat.abs_diff(incoming_position))
            .min()
        else {
            continue;
        };
        pairs += 1;
        max_error = Some(max_error.map_or(error, |current: Duration| current.max(error)));
    }

    BeatPhaseReport {
        pairs,
        max_error: (pairs > 0).then_some(max_error).flatten(),
    }
}

fn phrase_phase_report(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    plan: &TransitionPlan,
    length: PhraseLength,
) -> BeatPhaseReport {
    let outgoing_end = plan.outgoing_start.saturating_add(plan.duration);
    let incoming_end = plan
        .incoming_start
        .saturating_add(incoming_mix_source(plan));
    let outgoing_phrases =
        phrase_positions_between(outgoing, plan.outgoing_start, outgoing_end, length);
    let incoming_phrases =
        phrase_positions_between(incoming, plan.incoming_start, incoming_end, length);
    let mut pairs = 0;
    let mut max_error = None;

    for outgoing_phrase in outgoing_phrases {
        let output_elapsed = outgoing_phrase.saturating_sub(plan.outgoing_start);
        let incoming_position = plan
            .incoming_start
            .saturating_add(incoming_source_elapsed(plan, output_elapsed));
        let Some(error) = incoming_phrases
            .iter()
            .map(|phrase| phrase.abs_diff(incoming_position))
            .min()
        else {
            continue;
        };
        pairs += 1;
        max_error = Some(max_error.map_or(error, |current: Duration| current.max(error)));
    }

    BeatPhaseReport {
        pairs,
        max_error: (pairs > 0).then_some(max_error).flatten(),
    }
}

fn beat_handoff_phase_error(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    plan: &TransitionPlan,
) -> Option<Duration> {
    if let Some(error) = beat_marker_handoff_phase_error(outgoing, incoming, plan) {
        return Some(error);
    }

    if outgoing.beat_confidence < MIN_PHASE_MARKER_CONFIDENCE
        || incoming.beat_confidence < MIN_PHASE_MARKER_CONFIDENCE
    {
        return None;
    }
    let outgoing_first = beat_phase_anchor(outgoing)?;
    let incoming_first = beat_phase_anchor(incoming)?;
    let outgoing_interval = beat_interval(outgoing)?;
    let incoming_interval = beat_interval(incoming)?;

    handoff_cycle_phase_error(
        outgoing_first,
        outgoing_interval,
        incoming_first,
        incoming_interval,
        plan,
    )
}

fn beat_marker_handoff_phase_error(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    plan: &TransitionPlan,
) -> Option<Duration> {
    let outgoing_handoff = plan.outgoing_start.saturating_add(plan.duration);
    let incoming_handoff = plan
        .incoming_start
        .saturating_add(incoming_mix_source(plan));
    let outgoing_markers =
        trusted_beat_markers_between(outgoing, plan.outgoing_start, outgoing_handoff);
    let incoming_markers =
        trusted_beat_markers_between(incoming, plan.incoming_start, incoming_handoff);
    if outgoing_markers.is_empty() || incoming_markers.is_empty() {
        return None;
    }
    let outgoing_marker = outgoing_markers
        .iter()
        .copied()
        .min_by_key(|marker| marker.abs_diff(outgoing_handoff))?;
    let endpoint_tolerance = beat_interval(outgoing)
        .map(|interval| interval.div_f64(2.0))
        .unwrap_or(MAX_BEATMATCH_PHASE_ERROR);
    if outgoing_marker.abs_diff(outgoing_handoff) > endpoint_tolerance {
        return None;
    }

    let output_elapsed = outgoing_marker.saturating_sub(plan.outgoing_start);
    let incoming_position = plan
        .incoming_start
        .saturating_add(incoming_source_elapsed(plan, output_elapsed));
    incoming_markers
        .iter()
        .map(|marker| marker.abs_diff(incoming_position))
        .min()
}

fn downbeat_handoff_phase_error(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    plan: &TransitionPlan,
) -> Option<Duration> {
    let outgoing_grid = outgoing.beat_grid()?;
    let incoming_grid = incoming.beat_grid()?;
    if outgoing_grid.downbeat_confidence < 0.25 || incoming_grid.downbeat_confidence < 0.25 {
        return None;
    }
    handoff_cycle_phase_error(
        outgoing_grid.first_downbeat,
        bar_interval(outgoing_grid)?,
        incoming_grid.first_downbeat,
        bar_interval(incoming_grid)?,
        plan,
    )
}

fn phrase_handoff_phase_error(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    plan: &TransitionPlan,
    length: PhraseLength,
) -> Option<Duration> {
    let outgoing_grid = outgoing.beat_grid()?;
    let incoming_grid = incoming.beat_grid()?;
    if outgoing_grid.downbeat_confidence < 0.25 || incoming_grid.downbeat_confidence < 0.25 {
        return None;
    }
    let outgoing_interval = phrase_interval(outgoing_grid, length);
    let incoming_interval = phrase_interval(incoming_grid, length);
    if outgoing_interval.is_zero() || incoming_interval.is_zero() {
        return None;
    }
    handoff_cycle_phase_error(
        outgoing_grid.first_downbeat,
        outgoing_interval,
        incoming_grid.first_downbeat,
        incoming_interval,
        plan,
    )
}

fn handoff_cycle_phase_error(
    outgoing_first: Duration,
    outgoing_interval: Duration,
    incoming_first: Duration,
    incoming_interval: Duration,
    plan: &TransitionPlan,
) -> Option<Duration> {
    let outgoing_handoff = plan.outgoing_start.saturating_add(plan.duration);
    let incoming_handoff = plan
        .incoming_start
        .saturating_add(incoming_mix_source(plan));
    let incoming_interval = closest_cycle_interval_family(outgoing_interval, incoming_interval)
        .unwrap_or(incoming_interval);
    let outgoing_phase = cycle_phase_fraction(outgoing_handoff, outgoing_first, outgoing_interval)?;
    let incoming_phase = cycle_phase_fraction(incoming_handoff, incoming_first, incoming_interval)?;
    let delta = (outgoing_phase - incoming_phase).abs();
    let wrapped_delta = delta.min(1.0 - delta);
    Some(outgoing_interval.mul_f64(wrapped_delta))
}

fn closest_cycle_interval_family(
    outgoing_interval: Duration,
    incoming_interval: Duration,
) -> Option<Duration> {
    if outgoing_interval.is_zero() || incoming_interval.is_zero() {
        return None;
    }
    let outgoing = outgoing_interval.as_secs_f64();
    let incoming = incoming_interval.as_secs_f64();
    if !outgoing.is_finite() || !incoming.is_finite() || outgoing <= 0.0 || incoming <= 0.0 {
        return None;
    }
    [incoming, incoming * 2.0, incoming * 0.5]
        .into_iter()
        .filter(|candidate| candidate.is_finite() && *candidate > 0.0)
        .min_by(|left, right| (left - outgoing).abs().total_cmp(&(right - outgoing).abs()))
        .map(Duration::from_secs_f64)
}

fn phrase_phase_is_actionable(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    plan: &TransitionPlan,
    length: PhraseLength,
) -> bool {
    let Some(outgoing_grid) = outgoing.beat_grid() else {
        return false;
    };
    let Some(incoming_grid) = incoming.beat_grid() else {
        return false;
    };
    let outgoing_phrase = phrase_interval(outgoing_grid, length);
    let incoming_phrase = phrase_interval(incoming_grid, length);
    if outgoing_phrase.is_zero() || incoming_phrase.is_zero() {
        return false;
    }
    let minimum_overlap = outgoing_phrase.min(incoming_phrase).div_f64(2.0);
    plan.duration >= minimum_overlap
}

fn beat_positions_between(
    analysis: &TrackAnalysis,
    start: Duration,
    end: Duration,
) -> Vec<Duration> {
    if end <= start {
        return Vec::new();
    }

    let markers = trusted_beat_markers_between(analysis, start, end);
    if !markers.is_empty() {
        return markers;
    }

    let Some(bpm) = analysis.bpm else {
        return Vec::new();
    };
    if bpm <= 0.0 || !bpm.is_finite() || analysis.beat_confidence < MIN_PHASE_MARKER_CONFIDENCE {
        return Vec::new();
    }
    let interval = Duration::from_secs_f32(60.0 / bpm);
    if interval.is_zero() {
        return Vec::new();
    }

    let mut beat = align_to_global_beat_at_or_after(start, analysis);
    let mut beats = Vec::new();
    while beat <= end {
        beats.push(beat);
        beat += interval;
    }
    beats
}

fn trusted_beat_markers_between(
    analysis: &TrackAnalysis,
    start: Duration,
    end: Duration,
) -> Vec<Duration> {
    analysis
        .beat_markers
        .iter()
        .copied()
        .enumerate()
        .filter(|(index, beat)| {
            *beat >= start
                && *beat <= end
                && marker_confidence(analysis, *index) >= MIN_PHASE_MARKER_CONFIDENCE
        })
        .map(|(_, beat)| beat)
        .collect()
}

fn downbeat_positions_between(
    analysis: &TrackAnalysis,
    start: Duration,
    end: Duration,
) -> Vec<Duration> {
    if end <= start {
        return Vec::new();
    }
    let Some(grid) = analysis.beat_grid() else {
        return Vec::new();
    };
    if grid.downbeat_confidence < 0.25 {
        return Vec::new();
    }

    let interval = grid.beat_interval.mul_f64(f64::from(grid.beats_per_bar));
    if interval.is_zero() {
        return Vec::new();
    }

    cycle_positions_between(grid.first_downbeat, interval, start, end)
}

fn phrase_positions_between(
    analysis: &TrackAnalysis,
    start: Duration,
    end: Duration,
    length: PhraseLength,
) -> Vec<Duration> {
    if end <= start {
        return Vec::new();
    }
    let Some(grid) = analysis.beat_grid() else {
        return Vec::new();
    };
    if grid.downbeat_confidence < 0.25 {
        return Vec::new();
    }
    let interval = phrase_interval(grid, length);
    if interval.is_zero() {
        return Vec::new();
    }

    cycle_positions_between(grid.first_downbeat, interval, start, end)
}

fn cycle_positions_between(
    first: Duration,
    interval: Duration,
    start: Duration,
    end: Duration,
) -> Vec<Duration> {
    let interval_secs = interval.as_secs_f64();
    let first_secs = first.as_secs_f64();
    let start_secs = start.as_secs_f64();
    if interval_secs <= 0.0
        || !interval_secs.is_finite()
        || !first_secs.is_finite()
        || !start_secs.is_finite()
    {
        return Vec::new();
    }

    let cycles = ((start_secs - first_secs) / interval_secs).ceil();
    let mut position = first_secs + cycles * interval_secs;
    if position < 0.0 {
        position = first_secs;
        while position < start_secs {
            position += interval_secs;
        }
    }

    let mut positions = Vec::new();
    while position <= end.as_secs_f64() {
        positions.push(Duration::from_secs_f64(position));
        position += interval_secs;
    }
    positions
}

fn beat_interval(analysis: &TrackAnalysis) -> Option<Duration> {
    let bpm = analysis.bpm?;
    if bpm <= 0.0 || !bpm.is_finite() {
        return None;
    }
    let interval = Duration::from_secs_f32(60.0 / bpm);
    (!interval.is_zero()).then_some(interval)
}

fn beat_phase_anchor(analysis: &TrackAnalysis) -> Option<Duration> {
    analysis
        .beat_markers
        .iter()
        .copied()
        .enumerate()
        .filter(|(index, beat)| {
            *beat >= analysis.audible_start
                && marker_confidence(analysis, *index) >= MIN_PHASE_MARKER_CONFIDENCE
        })
        .map(|(_, beat)| beat)
        .min()
        .or_else(|| {
            analysis
                .beat_markers
                .iter()
                .copied()
                .enumerate()
                .filter(|(index, _)| {
                    marker_confidence(analysis, *index) >= MIN_PHASE_MARKER_CONFIDENCE
                })
                .map(|(_, beat)| beat)
                .min()
        })
        .or(analysis.first_beat)
        .or(analysis.first_downbeat)
}

fn low_handoff_range(plan: &TransitionPlan) -> (Option<f32>, Option<f32>) {
    if plan.kind == TransitionKind::Gapless || plan.duration.is_zero() {
        return (None, None);
    }

    let outgoing = EqTransition {
        id: 0,
        source_start: plan.outgoing_start,
        duration: plan.duration,
        role: EqTransitionRole::Outgoing,
    };
    let incoming = EqTransition {
        id: 0,
        source_start: plan.incoming_start,
        duration: incoming_mix_source(plan),
        role: EqTransitionRole::Incoming,
    };
    let mut min_gain = f32::INFINITY;
    let mut max_gain = f32::NEG_INFINITY;
    for sample in 0..=32 {
        let elapsed = plan.duration.mul_f64(f64::from(sample) / 32.0);
        let outgoing_position = plan.outgoing_start.saturating_add(elapsed);
        let incoming_position = plan
            .incoming_start
            .saturating_add(incoming_source_elapsed(plan, elapsed));
        let gain =
            outgoing.gains_at(outgoing_position).low + incoming.gains_at(incoming_position).low;
        min_gain = min_gain.min(gain);
        max_gain = max_gain.max(gain);
    }
    (Some(min_gain), Some(max_gain))
}

#[derive(Clone, Copy)]
struct DualVocalOverlapReport {
    samples: usize,
    max_risk: Option<f32>,
}

fn dual_vocal_overlap_report(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    plan: &TransitionPlan,
) -> DualVocalOverlapReport {
    const SAMPLES: u32 = 32;
    if plan.kind == TransitionKind::Gapless || plan.duration.is_zero() {
        return DualVocalOverlapReport {
            samples: 0,
            max_risk: None,
        };
    }
    if !summarize_vocals(outgoing).known || !summarize_vocals(incoming).known {
        return DualVocalOverlapReport {
            samples: 0,
            max_risk: None,
        };
    }

    let mut samples = 0;
    let mut max_risk = None;
    for index in 0..=SAMPLES {
        let elapsed = plan.duration.mul_f64(f64::from(index) / f64::from(SAMPLES));
        let outgoing_position = plan.outgoing_start.saturating_add(elapsed);
        let incoming_position = plan
            .incoming_start
            .saturating_add(incoming_source_elapsed(plan, elapsed));
        if outgoing_position < outgoing.audible_start
            || outgoing_position > outgoing.audible_end
            || incoming_position < incoming.audible_start
            || incoming_position > incoming.audible_end
        {
            continue;
        }

        let progress = elapsed.as_secs_f32() / plan.duration.as_secs_f32();
        let (outgoing_mix_gain, incoming_mix_gain) = equal_power_mix_gains(progress);
        let outgoing_eq = EqTransition {
            id: 0,
            source_start: plan.outgoing_start,
            duration: plan.duration,
            role: EqTransitionRole::Outgoing,
        }
        .gains_at(outgoing_position);
        let incoming_eq = EqTransition {
            id: 0,
            source_start: plan.incoming_start,
            duration: incoming_mix_source(plan),
            role: EqTransitionRole::Incoming,
        }
        .gains_at(incoming_position);
        let outgoing_risk = effective_vocal_risk(outgoing, outgoing_position)
            * outgoing_mix_gain
            * vocal_band_gain(outgoing_eq);
        let incoming_risk = effective_vocal_risk(incoming, incoming_position)
            * incoming_mix_gain
            * plan.incoming_gain
            * vocal_band_gain(incoming_eq);
        let risk = outgoing_risk.min(incoming_risk);
        samples += 1;
        max_risk = Some(max_risk.map_or(risk, |current: f32| current.max(risk)));
    }

    DualVocalOverlapReport {
        samples,
        max_risk: (samples > 0).then_some(max_risk).flatten(),
    }
}

#[derive(Clone, Copy)]
struct TransitionEnergyReport {
    samples: usize,
    min_ratio: Option<f32>,
    max_ratio: Option<f32>,
}

fn transition_energy_report(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    plan: &TransitionPlan,
) -> TransitionEnergyReport {
    const SAMPLES: u32 = 32;
    if plan.kind == TransitionKind::Gapless || plan.duration.is_zero() {
        return TransitionEnergyReport {
            samples: 0,
            min_ratio: None,
            max_ratio: None,
        };
    }
    let outgoing_reference = energy_at(outgoing, plan.outgoing_start);
    let incoming_reference = energy_at(
        incoming,
        plan.incoming_start
            .saturating_add(incoming_mix_source(plan)),
    )
    .map(|energy| energy * plan.incoming_gain);
    let Some(reference) = outgoing_reference
        .zip(incoming_reference)
        .map(|(outgoing, incoming)| outgoing.max(incoming).max(1.0e-6))
    else {
        return TransitionEnergyReport {
            samples: 0,
            min_ratio: None,
            max_ratio: None,
        };
    };

    let mut samples = 0;
    let mut min_ratio = f32::INFINITY;
    let mut max_ratio = f32::NEG_INFINITY;
    for index in 0..=SAMPLES {
        let elapsed = plan.duration.mul_f64(f64::from(index) / f64::from(SAMPLES));
        let outgoing_position = plan.outgoing_start.saturating_add(elapsed);
        let incoming_position = plan
            .incoming_start
            .saturating_add(incoming_source_elapsed(plan, elapsed));
        let (Some(outgoing_energy), Some(incoming_energy)) = (
            energy_at(outgoing, outgoing_position),
            energy_at(incoming, incoming_position),
        ) else {
            continue;
        };

        let progress = elapsed.as_secs_f32() / plan.duration.as_secs_f32();
        let (outgoing_mix_gain, incoming_mix_gain) = equal_power_mix_gains(progress);
        let outgoing_eq = EqTransition {
            id: 0,
            source_start: plan.outgoing_start,
            duration: plan.duration,
            role: EqTransitionRole::Outgoing,
        }
        .gains_at(outgoing_position);
        let incoming_eq = EqTransition {
            id: 0,
            source_start: plan.incoming_start,
            duration: incoming_mix_source(plan),
            role: EqTransitionRole::Incoming,
        }
        .gains_at(incoming_position);
        let outgoing_level = outgoing_energy * outgoing_mix_gain * full_band_gain(outgoing_eq);
        let incoming_level =
            incoming_energy * incoming_mix_gain * plan.incoming_gain * full_band_gain(incoming_eq);
        let combined = (outgoing_level.mul_add(outgoing_level, incoming_level * incoming_level))
            .sqrt()
            / reference;
        samples += 1;
        min_ratio = min_ratio.min(combined);
        max_ratio = max_ratio.max(combined);
    }

    TransitionEnergyReport {
        samples,
        min_ratio: (samples > 0).then_some(min_ratio),
        max_ratio: (samples > 0).then_some(max_ratio),
    }
}

#[allow(clippy::too_many_arguments)]
fn select_energy_balanced_start(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    incoming_start: Duration,
    default_start: Duration,
    target: Duration,
    search_window: Duration,
    beat_aligned: bool,
    phase_bias: BarPhaseBias,
    harmonic_compatibility: Option<f32>,
    incoming_gain: f32,
    tempo_curve: Option<(f32, f32)>,
    max_tempo_adjustment: f32,
) -> (Duration, Option<AutoMixEnergySelection>) {
    if !has_energy_profile(outgoing) || !has_energy_profile(incoming) {
        return (default_start, None);
    }

    let mut candidates = vec![default_start];
    if beat_aligned {
        if harmonic_compatibility.is_none_or(|score| score >= 0.5)
            && incoming.downbeat_confidence >= 0.25
            && incoming.first_downbeat.is_some()
        {
            candidates.extend(matching_phrase_phase_candidates(
                outgoing,
                incoming,
                incoming_start,
                target,
                search_window,
                PhraseLength::FourBars,
                phase_bias,
            ));
        }
        candidates.extend(matching_bar_phase_candidates(
            outgoing,
            incoming,
            incoming_start,
            target,
            search_window,
            phase_bias,
        ));
        candidates.extend(beat_start_candidates(
            outgoing,
            target,
            search_window,
            phase_bias,
        ));
    } else {
        candidates.extend(energy_grid_start_candidates(
            outgoing,
            target,
            search_window,
            phase_bias,
        ));
    }

    candidates.sort_unstable();
    candidates.dedup();

    let Some((mut best_start, mut best_score)) = transition_start_energy_score(
        outgoing,
        incoming,
        incoming_start,
        default_start,
        beat_aligned,
        harmonic_compatibility,
        incoming_gain,
        tempo_curve,
        max_tempo_adjustment,
    ) else {
        return (default_start, None);
    };
    let mut candidates_checked = 1;

    for candidate in candidates
        .into_iter()
        .filter(|candidate| *candidate != default_start)
        .take(MAX_ENERGY_START_CANDIDATES)
    {
        let Some((candidate_start, candidate_score)) = transition_start_energy_score(
            outgoing,
            incoming,
            incoming_start,
            candidate,
            beat_aligned,
            harmonic_compatibility,
            incoming_gain,
            tempo_curve,
            max_tempo_adjustment,
        ) else {
            continue;
        };
        candidates_checked += 1;
        if candidate_score + ENERGY_SELECTION_EPSILON < best_score {
            best_start = candidate_start;
            best_score = candidate_score;
        }
    }

    (
        best_start,
        Some(AutoMixEnergySelection {
            default_start,
            selected_start: best_start,
            candidates_checked,
        }),
    )
}

#[allow(clippy::too_many_arguments)]
fn transition_start_energy_score(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    incoming_start: Duration,
    outgoing_start: Duration,
    beat_aligned: bool,
    harmonic_compatibility: Option<f32>,
    incoming_gain: f32,
    tempo_curve: Option<(f32, f32)>,
    max_tempo_adjustment: f32,
) -> Option<(Duration, f32)> {
    let duration = outgoing.audible_end.saturating_sub(outgoing_start);
    if duration < MIN_AUDIBLE_MIX_OVERLAP {
        return None;
    }

    let (incoming_tempo_ratio, tempo_envelope) = build_tempo_envelope(
        outgoing,
        incoming,
        outgoing_start,
        incoming_start,
        duration,
        tempo_curve,
        max_tempo_adjustment,
    );
    let incoming_end = incoming_start.saturating_add(
        tempo_envelope.map_or(duration, |envelope| envelope.source_elapsed(duration)),
    );
    if incoming_end
        > incoming
            .audible_end
            .saturating_add(Duration::from_millis(20))
    {
        return None;
    }
    if vocal_overlap_limit(outgoing, incoming, incoming_start, duration, tempo_envelope) < duration
    {
        return None;
    }

    let plan = TransitionPlan {
        kind: if beat_aligned {
            TransitionKind::BeatMatched
        } else {
            TransitionKind::Crossfade
        },
        outgoing_start,
        incoming_start,
        duration,
        incoming_tempo_ratio,
        harmonic_compatibility,
        incoming_gain,
        tempo_envelope,
        energy_selection: None,
    };
    let quality = evaluate_transition_quality(outgoing, incoming, &plan);
    if quality.has_blocking_issue() {
        return None;
    }
    energy_balance_score(&quality).map(|score| (outgoing_start, score))
}

fn energy_balance_score(quality: &AutoMixQualityReport) -> Option<f32> {
    if quality.energy_samples_checked == 0 {
        return None;
    }
    let min_ratio = quality.min_mix_energy_ratio?.max(1.0e-6);
    let max_ratio = quality.max_mix_energy_ratio?.max(1.0e-6);
    if !min_ratio.is_finite() || !max_ratio.is_finite() {
        return None;
    }

    let dip_penalty = (0.9 - min_ratio).max(0.0) * 4.0;
    let buildup_penalty = (max_ratio - 1.15).max(0.0) * 2.0;
    let movement_penalty = min_ratio.ln().abs() * 0.15 + max_ratio.ln().abs() * 0.05;
    Some(dip_penalty + buildup_penalty + movement_penalty)
}

fn has_energy_profile(analysis: &TrackAnalysis) -> bool {
    analysis.energy_profile_rate > 0 && !analysis.energy_profile.is_empty()
}

fn candidate_bounds(
    target: Duration,
    search_window: Duration,
    bias: BarPhaseBias,
) -> (Duration, Duration) {
    match bias {
        BarPhaseBias::AtOrBefore => (target.saturating_sub(search_window), target),
        BarPhaseBias::AtOrAfter => (target, target.saturating_add(search_window)),
    }
}

fn matching_phrase_phase_candidates(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    incoming_start: Duration,
    target: Duration,
    search_window: Duration,
    length: PhraseLength,
    bias: BarPhaseBias,
) -> Vec<Duration> {
    let Some(outgoing_grid) = outgoing.beat_grid() else {
        return Vec::new();
    };
    let Some(incoming_grid) = incoming.beat_grid() else {
        return Vec::new();
    };
    if outgoing_grid.downbeat_confidence < 0.25 || incoming_grid.downbeat_confidence < 0.25 {
        return Vec::new();
    }

    let outgoing_phrase = phrase_interval(outgoing_grid, length);
    let incoming_phrase = phrase_interval(incoming_grid, length);
    if outgoing_phrase.is_zero() || incoming_phrase.is_zero() {
        return Vec::new();
    }
    let Some(incoming_phase) = cycle_phase_fraction(
        incoming_start,
        incoming_grid.first_downbeat,
        incoming_phrase,
    ) else {
        return Vec::new();
    };
    let (earliest, latest) = candidate_bounds(target, search_window, bias);
    matching_cycle_phase_candidates(
        outgoing_grid.first_downbeat,
        outgoing_phrase,
        incoming_phase,
        earliest,
        latest,
    )
    .into_iter()
    .filter(|candidate| {
        valid_outgoing_start(outgoing, *candidate)
            && has_trusted_marker_near(outgoing, *candidate, MAX_BEATMATCH_PHASE_ERROR)
    })
    .collect()
}

fn matching_bar_phase_candidates(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    incoming_start: Duration,
    target: Duration,
    search_window: Duration,
    bias: BarPhaseBias,
) -> Vec<Duration> {
    let Some(outgoing_grid) = outgoing.beat_grid() else {
        return Vec::new();
    };
    let Some(incoming_grid) = incoming.beat_grid() else {
        return Vec::new();
    };
    if outgoing_grid.downbeat_confidence < 0.25 || incoming_grid.downbeat_confidence < 0.25 {
        return Vec::new();
    }

    let Some(outgoing_bar) = bar_interval(outgoing_grid) else {
        return Vec::new();
    };
    let Some(incoming_bar) = bar_interval(incoming_grid) else {
        return Vec::new();
    };
    let Some(incoming_phase) =
        cycle_phase_fraction(incoming_start, incoming_grid.first_downbeat, incoming_bar)
    else {
        return Vec::new();
    };
    let (earliest, latest) = candidate_bounds(target, search_window, bias);
    matching_cycle_phase_candidates(
        outgoing_grid.first_downbeat,
        outgoing_bar,
        incoming_phase,
        earliest,
        latest,
    )
    .into_iter()
    .filter(|candidate| {
        valid_outgoing_start(outgoing, *candidate)
            && has_trusted_marker_near(outgoing, *candidate, MAX_BEATMATCH_PHASE_ERROR)
    })
    .collect()
}

fn matching_cycle_phase_candidates(
    first: Duration,
    interval: Duration,
    phase: f64,
    earliest: Duration,
    latest: Duration,
) -> Vec<Duration> {
    let interval_secs = interval.as_secs_f64();
    let base = first.as_secs_f64() + interval_secs * phase;
    let earliest_secs = earliest.as_secs_f64();
    let latest_secs = latest.as_secs_f64();
    if interval_secs <= 0.0
        || !interval_secs.is_finite()
        || !base.is_finite()
        || earliest_secs > latest_secs
    {
        return Vec::new();
    }

    let mut candidates = Vec::new();
    let mut cycle = ((earliest_secs - base) / interval_secs).ceil();
    while candidates.len() < MAX_ENERGY_START_CANDIDATES {
        let position = base + cycle * interval_secs;
        if position > latest_secs + 1.0e-6 {
            break;
        }
        if position >= earliest_secs - 1.0e-6 && position.is_finite() && position >= 0.0 {
            candidates.push(Duration::from_secs_f64(position));
        }
        cycle += 1.0;
    }
    candidates
}

fn beat_start_candidates(
    analysis: &TrackAnalysis,
    target: Duration,
    search_window: Duration,
    bias: BarPhaseBias,
) -> Vec<Duration> {
    let (earliest, latest) = candidate_bounds(target, search_window, bias);
    let mut candidates = analysis
        .beat_markers
        .iter()
        .copied()
        .enumerate()
        .filter(|(index, beat)| {
            marker_confidence(analysis, *index) >= MIN_PHASE_MARKER_CONFIDENCE
                && *beat >= earliest
                && *beat <= latest
        })
        .map(|(_, beat)| beat)
        .filter(|beat| valid_outgoing_start(analysis, *beat))
        .collect::<Vec<_>>();

    if let (Some(first), Some(bpm)) = (analysis.first_beat, analysis.bpm)
        && bpm.is_finite()
        && bpm > 0.0
    {
        let interval = Duration::from_secs_f32(60.0 / bpm);
        if !interval.is_zero() {
            let mut position = if earliest <= first {
                first
            } else {
                align_to_global_beat_at_or_after(earliest, analysis)
            };
            while position <= latest && candidates.len() < MAX_ENERGY_START_CANDIDATES {
                if valid_outgoing_start(analysis, position) {
                    candidates.push(position);
                }
                position += interval;
            }
        }
    }

    candidates
}

fn energy_grid_start_candidates(
    analysis: &TrackAnalysis,
    target: Duration,
    search_window: Duration,
    bias: BarPhaseBias,
) -> Vec<Duration> {
    const STEP: Duration = Duration::from_millis(500);
    let (earliest, latest) = candidate_bounds(target, search_window, bias);
    let mut candidates = Vec::new();
    let mut position = earliest;
    while position <= latest && candidates.len() < MAX_ENERGY_START_CANDIDATES {
        if valid_outgoing_start(analysis, position) {
            candidates.push(position);
        }
        position += STEP;
    }
    if valid_outgoing_start(analysis, latest) {
        candidates.push(latest);
    }
    candidates
}

fn valid_outgoing_start(analysis: &TrackAnalysis, candidate: Duration) -> bool {
    candidate >= analysis.audible_start
        && candidate <= analysis.audible_end
        && analysis.audible_end.saturating_sub(candidate) >= MIN_AUDIBLE_MIX_OVERLAP
}

fn equal_power_mix_gains(progress: f32) -> (f32, f32) {
    let angle = progress.clamp(0.0, 1.0) * std::f32::consts::FRAC_PI_2;
    (angle.cos(), angle.sin())
}

fn vocal_band_gain(gains: EqGains) -> f32 {
    (0.7 * gains.mid + 0.3 * gains.high).clamp(0.0, 1.0)
}

fn full_band_gain(gains: EqGains) -> f32 {
    (0.35 * gains.low + 0.45 * gains.mid + 0.2 * gains.high).clamp(0.0, 1.0)
}

fn energy_at(analysis: &TrackAnalysis, position: Duration) -> Option<f32> {
    if analysis.energy_profile_rate == 0 || analysis.energy_profile.is_empty() {
        return None;
    }
    let index = (position.as_secs_f64() * f64::from(analysis.energy_profile_rate)).floor() as usize;
    let value = analysis.energy_profile.get(index)?;
    let dbfs =
        MIN_ENERGY_PROFILE_DBFS + (f32::from(*value) / 255.0) * (0.0 - MIN_ENERGY_PROFILE_DBFS);
    Some(dbfs_to_linear(dbfs))
}

fn incoming_mix_source(plan: &TransitionPlan) -> Duration {
    incoming_source_elapsed(plan, plan.duration)
}

fn incoming_source_elapsed(plan: &TransitionPlan, output_elapsed: Duration) -> Duration {
    plan.tempo_envelope.map_or(output_elapsed, |envelope| {
        envelope.source_elapsed(output_elapsed)
    })
}

fn build_tempo_envelope(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    outgoing_start: Duration,
    incoming_start: Duration,
    duration: Duration,
    tempo_curve: Option<(f32, f32)>,
    max_tempo_adjustment: f32,
) -> (f32, Option<TempoEnvelope>) {
    let (tempo_start, tempo_end) = tempo_curve.unwrap_or((1.0, 1.0));
    let phase_segments = tempo_curve
        .and_then(|(tempo_start, _)| {
            phase_follow_segments(
                outgoing,
                incoming,
                outgoing_start,
                incoming_start,
                duration,
                tempo_start,
                max_tempo_adjustment,
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
    (envelope_start, tempo_envelope)
}

fn trusted_structure_overlap(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    maximum: Duration,
) -> Option<Duration> {
    const MINIMUM: Duration = Duration::from_secs(1);
    let outgoing_start = outgoing
        .outro_start
        .filter(|_| outgoing.outro_confidence >= 0.65)?;
    let incoming_end = incoming
        .intro_end
        .filter(|_| incoming.intro_confidence >= 0.65)?;
    let outgoing_span = outgoing.audible_end.saturating_sub(outgoing_start);
    let incoming_span = incoming_end.saturating_sub(incoming.audible_start);
    let duration = maximum.min(outgoing_span).min(incoming_span);
    (duration >= MINIMUM).then_some(duration)
}

fn safe_incoming_beat_start(incoming: &TrackAnalysis) -> Option<Duration> {
    const MAX_AUDIBLE_PICKUP: Duration = Duration::from_millis(100);
    let candidate = first_trusted_downbeat_after_audible_start(incoming, MAX_AUDIBLE_PICKUP)
        .or_else(|| first_trusted_beat_marker_after_audible_start(incoming, MAX_AUDIBLE_PICKUP))
        .or(incoming.first_beat)?;
    (candidate >= incoming.audible_start
        && candidate.saturating_sub(incoming.audible_start) <= MAX_AUDIBLE_PICKUP)
        .then_some(candidate)
        .filter(|candidate| !known_vocal_risk_between(incoming, incoming.audible_start, *candidate))
}

fn first_trusted_downbeat_after_audible_start(
    analysis: &TrackAnalysis,
    maximum_pickup: Duration,
) -> Option<Duration> {
    let downbeat = analysis.first_downbeat?;
    (analysis.downbeat_confidence >= 0.25
        && downbeat >= analysis.audible_start
        && downbeat.saturating_sub(analysis.audible_start) <= maximum_pickup
        && has_trusted_marker_near(analysis, downbeat, MAX_BEATMATCH_PHASE_ERROR))
    .then_some(downbeat)
}

fn first_trusted_beat_marker_after_audible_start(
    analysis: &TrackAnalysis,
    maximum_pickup: Duration,
) -> Option<Duration> {
    analysis
        .beat_markers
        .iter()
        .copied()
        .enumerate()
        .filter(|(index, beat)| {
            marker_confidence(analysis, *index) >= MIN_PHASE_MARKER_CONFIDENCE
                && *beat >= analysis.audible_start
                && beat.saturating_sub(analysis.audible_start) <= maximum_pickup
        })
        .map(|(_, beat)| beat)
        .min()
}

fn has_trusted_marker_near(
    analysis: &TrackAnalysis,
    position: Duration,
    tolerance: Duration,
) -> bool {
    if analysis.beat_markers.is_empty() {
        return true;
    }
    analysis
        .beat_markers
        .iter()
        .copied()
        .enumerate()
        .any(|(index, beat)| {
            marker_confidence(analysis, index) >= MIN_PHASE_MARKER_CONFIDENCE
                && beat.abs_diff(position) <= tolerance
        })
}

#[derive(Clone, Copy)]
struct VocalSummary {
    known: bool,
    first: Option<Duration>,
    last_end: Option<Duration>,
}

fn summarize_vocals(analysis: &TrackAnalysis) -> VocalSummary {
    if analysis.vocal_activity_rate == 0
        || analysis.vocal_activity.len() != analysis.vocal_activity_confidences.len()
        || analysis.vocal_activity.is_empty()
    {
        return VocalSummary {
            known: false,
            first: None,
            last_end: None,
        };
    }
    let confidence = analysis
        .vocal_activity_confidences
        .iter()
        .map(|value| f32::from(*value) / 255.0)
        .sum::<f32>()
        / analysis.vocal_activity_confidences.len() as f32;
    if confidence < 0.6 {
        return VocalSummary {
            known: false,
            first: None,
            last_end: None,
        };
    }
    let rate = f64::from(analysis.vocal_activity_rate);
    let mut active = analysis
        .vocal_activity
        .iter()
        .enumerate()
        .filter_map(|(index, _)| {
            let bin_start = Duration::from_secs_f64(index as f64 / rate);
            let bin_end = Duration::from_secs_f64((index + 1) as f64 / rate);
            (bin_start < analysis.audible_end
                && bin_end > analysis.audible_start
                && effective_vocal_risk(analysis, bin_start) >= 0.58)
                .then_some(index)
        });
    let first = active.next();
    let last = active.next_back().or(first);
    VocalSummary {
        known: true,
        first: first
            .map(|index| Duration::from_secs_f64(index as f64 / rate).max(analysis.audible_start)),
        last_end: last.map(|index| {
            Duration::from_secs_f64((index + 1) as f64 / rate).min(analysis.audible_end)
        }),
    }
}

fn known_vocal_risk_between(analysis: &TrackAnalysis, start: Duration, end: Duration) -> bool {
    let summary = summarize_vocals(analysis);
    if !summary.known || end <= start {
        return false;
    }
    let rate = f64::from(analysis.vocal_activity_rate);
    let first = (start.as_secs_f64() * rate).floor() as usize;
    let last = (end.as_secs_f64() * rate).ceil() as usize;
    (first..last.min(analysis.vocal_activity.len())).any(|index| {
        effective_vocal_risk(analysis, Duration::from_secs_f64(index as f64 / rate)) >= 0.58
    })
}

fn vocal_overlap_limit(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    incoming_start: Duration,
    maximum: Duration,
    tempo_envelope: Option<TempoEnvelope>,
) -> Duration {
    let outgoing_vocals = summarize_vocals(outgoing);
    let incoming_vocals = summarize_vocals(incoming);
    if !outgoing_vocals.known || !incoming_vocals.known {
        return maximum.min(Duration::from_secs(2));
    }
    let (Some(last_outgoing_vocal), Some(first_incoming_vocal)) =
        (outgoing_vocals.last_end, incoming_vocals.first)
    else {
        return maximum;
    };
    let outgoing_tail = outgoing
        .audible_end
        .saturating_sub(last_outgoing_vocal)
        .min(maximum);
    let incoming_head_source = first_incoming_vocal.saturating_sub(incoming_start);
    let incoming_head = tempo_envelope.map_or(incoming_head_source, |envelope| {
        envelope.output_elapsed(incoming_head_source)
    });
    maximum.min(outgoing_tail.saturating_add(incoming_head))
}

fn align_to_beat_at_or_after(position: Duration, analysis: &TrackAnalysis) -> Duration {
    let fallback = align_to_global_beat_at_or_after(position, analysis);
    analysis
        .beat_markers
        .iter()
        .copied()
        .enumerate()
        .filter(|(index, _)| marker_confidence(analysis, *index) >= MIN_PHASE_MARKER_CONFIDENCE)
        .map(|(_, beat)| beat)
        .filter(|beat| *beat >= position)
        .min_by_key(|beat| beat.abs_diff(fallback))
        .filter(|beat| beat.abs_diff(fallback) <= marker_snap_tolerance(analysis))
        .unwrap_or(fallback)
        .min(analysis.audible_end)
}

fn align_to_global_beat_at_or_after(position: Duration, analysis: &TrackAnalysis) -> Duration {
    let (Some(first), Some(bpm)) = (analysis.first_beat, analysis.bpm) else {
        return position;
    };
    if bpm <= 0.0 || position <= first {
        return first.max(position);
    }
    let interval = Duration::from_secs_f32(60.0 / bpm);
    if interval.is_zero() {
        return position;
    }
    let beats = position.saturating_sub(first).as_secs_f64() / interval.as_secs_f64();
    first + interval.mul_f64(beats.ceil())
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

fn align_to_matching_phrase_phase(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    incoming_start: Duration,
    target: Duration,
    search_window: Duration,
    length: PhraseLength,
    bias: BarPhaseBias,
) -> Option<Duration> {
    let outgoing_grid = outgoing.beat_grid()?;
    let incoming_grid = incoming.beat_grid()?;
    if outgoing_grid.downbeat_confidence < 0.25 || incoming_grid.downbeat_confidence < 0.25 {
        return None;
    }

    let outgoing_phrase = phrase_interval(outgoing_grid, length);
    let incoming_phrase = phrase_interval(incoming_grid, length);
    if outgoing_phrase.is_zero() || incoming_phrase.is_zero() {
        return None;
    }
    let incoming_phase = cycle_phase_fraction(
        incoming_start,
        incoming_grid.first_downbeat,
        incoming_phrase,
    )?;
    let candidate = align_to_matching_cycle_phase(
        outgoing_grid.first_downbeat,
        outgoing_phrase,
        incoming_phase,
        target,
        bias,
    )?;
    let earliest = target.saturating_sub(search_window);
    let latest = target.saturating_add(search_window);
    let inside_search = match bias {
        BarPhaseBias::AtOrBefore => candidate >= earliest && candidate <= target,
        BarPhaseBias::AtOrAfter => candidate >= target && candidate <= latest,
    };
    (inside_search
        && candidate >= outgoing.audible_start
        && candidate <= outgoing.audible_end
        && outgoing.audible_end.saturating_sub(candidate) >= MIN_AUDIBLE_MIX_OVERLAP
        && has_trusted_marker_near(outgoing, candidate, MAX_BEATMATCH_PHASE_ERROR))
    .then_some(candidate)
}

#[derive(Clone, Copy)]
enum BarPhaseBias {
    AtOrBefore,
    AtOrAfter,
}

fn align_to_matching_bar_phase(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    incoming_start: Duration,
    target: Duration,
    bias: BarPhaseBias,
) -> Option<Duration> {
    let outgoing_grid = outgoing.beat_grid()?;
    let incoming_grid = incoming.beat_grid()?;
    if outgoing_grid.downbeat_confidence < 0.25 || incoming_grid.downbeat_confidence < 0.25 {
        return None;
    }

    let outgoing_bar = bar_interval(outgoing_grid)?;
    let incoming_bar = bar_interval(incoming_grid)?;
    let incoming_phase =
        cycle_phase_fraction(incoming_start, incoming_grid.first_downbeat, incoming_bar)?;
    let candidate = align_to_matching_cycle_phase(
        outgoing_grid.first_downbeat,
        outgoing_bar,
        incoming_phase,
        target,
        bias,
    )?;
    (candidate >= outgoing.audible_start
        && candidate <= outgoing.audible_end
        && has_trusted_marker_near(outgoing, candidate, MAX_BEATMATCH_PHASE_ERROR))
    .then_some(candidate)
}

fn bar_interval(grid: BeatGrid) -> Option<Duration> {
    let interval = grid.beat_interval.mul_f64(f64::from(grid.beats_per_bar));
    (!interval.is_zero()).then_some(interval)
}

fn phrase_interval(grid: BeatGrid, length: PhraseLength) -> Duration {
    grid.beat_interval
        .mul_f64(f64::from(u32::from(grid.beats_per_bar) * length.bars()))
}

fn cycle_phase_fraction(position: Duration, first: Duration, interval: Duration) -> Option<f64> {
    let interval = interval.as_secs_f64();
    if interval <= 0.0 || !interval.is_finite() {
        return None;
    }
    let delta = position.as_secs_f64() - first.as_secs_f64();
    Some(delta.rem_euclid(interval) / interval)
}

fn align_to_matching_cycle_phase(
    first: Duration,
    interval: Duration,
    phase: f64,
    target: Duration,
    bias: BarPhaseBias,
) -> Option<Duration> {
    let interval_secs = interval.as_secs_f64();
    let base = first.as_secs_f64() + interval_secs * phase;
    let target = target.as_secs_f64();
    if interval_secs <= 0.0
        || !interval_secs.is_finite()
        || !base.is_finite()
        || !target.is_finite()
    {
        return None;
    }

    let cycles = match bias {
        BarPhaseBias::AtOrBefore => ((target - base) / interval_secs).floor(),
        BarPhaseBias::AtOrAfter => ((target - base) / interval_secs).ceil(),
    };
    let candidate = base + cycles * interval_secs;
    (candidate.is_finite() && candidate >= 0.0).then(|| Duration::from_secs_f64(candidate))
}

fn align_to_beat(position: Duration, analysis: &TrackAnalysis) -> Duration {
    let fallback = align_to_global_beat(position, analysis);
    analysis
        .beat_markers
        .iter()
        .copied()
        .enumerate()
        .filter(|(index, _)| marker_confidence(analysis, *index) >= MIN_PHASE_MARKER_CONFIDENCE)
        .map(|(_, beat)| beat)
        .filter(|beat| *beat <= position)
        .min_by_key(|beat| beat.abs_diff(fallback))
        .filter(|beat| beat.abs_diff(fallback) <= marker_snap_tolerance(analysis))
        .unwrap_or(fallback)
}

fn align_to_global_beat(position: Duration, analysis: &TrackAnalysis) -> Duration {
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
        .enumerate()
        .filter(|(index, _)| marker_confidence(analysis, *index) >= MIN_PHASE_MARKER_CONFIDENCE)
        .map(|(_, beat)| beat)
        .min_by_key(|beat| beat.abs_diff(position))
        .filter(|beat| beat.abs_diff(position) <= marker_snap_tolerance(analysis))
}

fn marker_snap_tolerance(analysis: &TrackAnalysis) -> Duration {
    analysis
        .bpm
        .filter(|bpm| bpm.is_finite() && *bpm > 0.0)
        .map(|bpm| Duration::from_secs_f32(15.0 / bpm))
        .unwrap_or(Duration::from_millis(100))
}

fn marker_confidence(analysis: &TrackAnalysis, index: usize) -> f32 {
    analysis
        .beat_marker_confidences
        .get(index)
        .copied()
        .unwrap_or(analysis.beat_confidence)
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
        energy_selection: None,
    }
}

fn conservative_crossfade_plan(
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
    let mut duration = if harmonic_compatibility.is_some_and(|score| score < 0.5) {
        timing.fade_duration.min(Duration::from_secs(4))
    } else {
        timing.fade_duration
    };
    duration = duration.min(vocal_overlap_limit(
        outgoing,
        incoming,
        incoming.audible_start,
        duration,
        None,
    ));
    if duration < MIN_AUDIBLE_MIX_OVERLAP {
        return gapless_plan(outgoing, incoming);
    }

    TransitionPlan {
        kind: TransitionKind::Crossfade,
        outgoing_start: outgoing.audible_end.saturating_sub(duration),
        incoming_start: incoming.audible_start,
        duration,
        incoming_tempo_ratio: 1.0,
        harmonic_compatibility,
        incoming_gain: recommended_incoming_gain(outgoing, incoming),
        tempo_envelope: None,
        energy_selection: None,
    }
}

fn recommended_incoming_gain(outgoing: &TrackAnalysis, incoming: &TrackAnalysis) -> f32 {
    let level_gain = outgoing
        .rms_dbfs
        .zip(incoming.rms_dbfs)
        .map_or(1.0, |(outgoing_level, incoming_level)| {
            dbfs_to_linear(outgoing_level - incoming_level)
        });
    let peak_gain = incoming
        .sample_peak_dbfs
        .map(|peak| dbfs_to_linear(MIX_PEAK_HEADROOM_DBFS - peak))
        .unwrap_or(1.0);
    let mix_peak_gain = combined_peak_safe_incoming_gain(outgoing, incoming).unwrap_or(1.0);
    level_gain
        .min(peak_gain)
        .min(mix_peak_gain)
        .clamp(0.25, 1.0)
}

fn combined_peak_safe_incoming_gain(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
) -> Option<f32> {
    const MIDPOINT_GAIN: f32 = std::f32::consts::FRAC_1_SQRT_2;
    let outgoing_peak = dbfs_to_linear(outgoing.sample_peak_dbfs?);
    let incoming_peak = dbfs_to_linear(incoming.sample_peak_dbfs?);
    if outgoing_peak <= 0.0 || incoming_peak <= 0.0 {
        return None;
    }
    let peak_limit = dbfs_to_linear(MIX_PEAK_HEADROOM_DBFS);
    let remaining = peak_limit - outgoing_peak * MIDPOINT_GAIN;
    if remaining <= 0.0 {
        return Some(0.25);
    }
    Some((remaining / (incoming_peak * MIDPOINT_GAIN)).clamp(0.25, 1.0))
}

fn dbfs_to_linear(dbfs: f32) -> f32 {
    10.0_f32.powf(dbfs / 20.0)
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
    if !tempo_alignment_confident(outgoing, config.min_beat_confidence)
        || !tempo_alignment_confident(incoming, config.min_beat_confidence)
    {
        return None;
    }
    let outgoing_bpm = outgoing.bpm?;
    let incoming_bpm = incoming.bpm?;
    if outgoing_bpm <= 0.0 || incoming_bpm <= 0.0 {
        return None;
    }

    let ratio = closest_tempo_family_ratio(outgoing_bpm, incoming_bpm)?;
    (ratio.is_finite() && (ratio - 1.0).abs() <= config.max_tempo_adjustment)
        .then_some((ratio, ratio))
}

fn closest_tempo_family_ratio(outgoing_bpm: f32, incoming_bpm: f32) -> Option<f32> {
    if outgoing_bpm <= 0.0 || incoming_bpm <= 0.0 {
        return None;
    }
    let raw = outgoing_bpm / incoming_bpm;
    [raw, raw * 2.0, raw * 0.5]
        .iter()
        .copied()
        .filter(|ratio| ratio.is_finite() && *ratio > 0.0)
        .min_by(|left, right| (left - 1.0).abs().total_cmp(&(right - 1.0).abs()))
}

fn tempo_alignment_confident(analysis: &TrackAnalysis, min_beat_confidence: f32) -> bool {
    if analysis.beat_confidence >= min_beat_confidence {
        return true;
    }
    if analysis.beat_confidence < MIN_MARKER_BACKED_BEAT_CONFIDENCE
        || analysis.beat_markers.len() < MIN_MARKER_BACKED_KICK_MARKERS
        || analysis.beat_marker_confidences.is_empty()
    {
        return false;
    }

    let trusted = analysis
        .beat_marker_confidences
        .iter()
        .filter(|confidence| **confidence >= MIN_PHASE_MARKER_CONFIDENCE)
        .count();
    trusted >= MIN_MARKER_BACKED_KICK_MARKERS
        && trusted as f32 / analysis.beat_marker_confidences.len() as f32
            >= MIN_MARKER_BACKED_KICK_COVERAGE
}

fn phase_follow_segments(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    outgoing_start: Duration,
    incoming_start: Duration,
    duration: Duration,
    global_speed: f32,
    max_tempo_adjustment: f32,
) -> Option<Vec<TempoSegment>> {
    const BEATS_PER_CORRECTION: usize = 4;
    if !global_speed.is_finite() || global_speed <= 0.0 {
        return None;
    }
    let outgoing_end = outgoing_start.saturating_add(duration);
    let outgoing_beats = outgoing
        .beat_markers
        .iter()
        .copied()
        .enumerate()
        .filter(|(_, beat)| *beat >= outgoing_start && *beat <= outgoing_end)
        .map(|(index, beat)| (beat, marker_confidence(outgoing, index)))
        .collect::<Vec<_>>();
    let incoming_beats = incoming
        .beat_markers
        .iter()
        .copied()
        .enumerate()
        .filter(|(_, beat)| *beat >= incoming_start)
        .map(|(index, beat)| (beat, marker_confidence(incoming, index)))
        .collect::<Vec<_>>();
    let paired_beats = pair_phase_follow_beats(
        &outgoing_beats,
        &incoming_beats,
        outgoing_start,
        incoming_start,
        global_speed,
        incoming,
    );
    let paired = paired_beats.len();
    if paired < 5
        || outgoing_beats[0].0.abs_diff(outgoing_start) > Duration::from_millis(30)
        || paired_beats[0].1.0.abs_diff(incoming_start) > Duration::from_millis(30)
    {
        return None;
    }

    let mut segments = Vec::new();
    let mut index = 0;
    let mut mapped_source = 0.0_f32;
    while index + 1 < paired && segments.len() < MAX_TEMPO_SEGMENTS {
        let end = (index + BEATS_PER_CORRECTION).min(paired - 1);
        let output_delta = paired_beats[end]
            .0
            .0
            .saturating_sub(paired_beats[index].0.0);
        let source_delta = paired_beats[end]
            .1
            .0
            .saturating_sub(paired_beats[index].1.0);
        if output_delta.is_zero() || source_delta.is_zero() {
            return None;
        }
        let trusted = paired_beats[index..=end]
            .iter()
            .filter(|((_, confidence), _)| *confidence >= MIN_PHASE_MARKER_CONFIDENCE)
            .count()
            >= 3
            && paired_beats[index..=end]
                .iter()
                .filter(|(_, (_, confidence))| *confidence >= MIN_PHASE_MARKER_CONFIDENCE)
                .count()
                >= 3;
        let trusted = trusted
            && paired_beats[index].0.1 >= MIN_PHASE_MARKER_CONFIDENCE
            && paired_beats[end].0.1 >= MIN_PHASE_MARKER_CONFIDENCE
            && paired_beats[index].1.1 >= MIN_PHASE_MARKER_CONFIDENCE
            && paired_beats[end].1.1 >= MIN_PHASE_MARKER_CONFIDENCE;
        let desired_source = paired_beats[end]
            .1
            .0
            .saturating_sub(incoming_start)
            .as_secs_f32();
        let corrected_speed = (desired_source - mapped_source) / output_delta.as_secs_f32();
        let speed = if trusted
            && corrected_speed.is_finite()
            && (corrected_speed - 1.0).abs() <= max_tempo_adjustment
        {
            corrected_speed
        } else {
            global_speed
        };
        segments.push(TempoSegment {
            output_end: paired_beats[end].0.0.saturating_sub(outgoing_start),
            speed,
        });
        mapped_source += output_delta.as_secs_f32() * speed;
        index = end;
    }
    (!segments.is_empty()).then_some(segments)
}

fn pair_phase_follow_beats(
    outgoing_beats: &[(Duration, f32)],
    incoming_beats: &[(Duration, f32)],
    outgoing_start: Duration,
    incoming_start: Duration,
    global_speed: f32,
    incoming: &TrackAnalysis,
) -> Vec<((Duration, f32), (Duration, f32))> {
    let tolerance = marker_snap_tolerance(incoming).max(MAX_BEATMATCH_PHASE_ERROR);
    let mut pairs = Vec::new();
    let mut next_incoming_index = 0;

    for outgoing in outgoing_beats {
        let output_elapsed = outgoing.0.saturating_sub(outgoing_start);
        let target_source = incoming_start.saturating_add(Duration::from_secs_f64(
            output_elapsed.as_secs_f64() * f64::from(global_speed),
        ));
        let Some((offset, (_, incoming))) = incoming_beats
            .iter()
            .enumerate()
            .skip(next_incoming_index)
            .map(|(index, beat)| (index, (beat.0.abs_diff(target_source), beat)))
            .min_by_key(|(_, (error, _))| *error)
        else {
            break;
        };
        if incoming.0.abs_diff(target_source) > tolerance {
            continue;
        }
        pairs.push((*outgoing, *incoming));
        next_incoming_index = offset + 1;
    }

    pairs
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

    #[test]
    fn equalizer_transition_hands_bass_to_incoming_deck_by_midpoint() {
        let start = Duration::from_secs(10);
        let duration = Duration::from_secs(8);
        let outgoing = EqTransition {
            id: 1,
            source_start: start,
            duration,
            role: EqTransitionRole::Outgoing,
        };
        let incoming = EqTransition {
            role: EqTransitionRole::Incoming,
            ..outgoing
        };

        assert_eq!(
            outgoing.gains_at(start),
            EqGains {
                low: 1.0,
                mid: 1.0,
                high: 1.0,
            }
        );
        assert_eq!(incoming.gains_at(start).low, 0.0);
        assert_eq!(incoming.gains_at(start).mid, 0.65);
        assert_eq!(incoming.gains_at(start).high, 0.8);

        let quarter = start + duration / 4;
        assert!((outgoing.gains_at(quarter).low - 0.5).abs() < f32::EPSILON);
        assert!((incoming.gains_at(quarter).low - 0.5).abs() < f32::EPSILON);

        let midpoint = start + duration / 2;
        assert_eq!(
            outgoing.gains_at(midpoint),
            EqGains {
                low: 0.0,
                mid: 1.0,
                high: 1.0,
            }
        );
        assert_eq!(
            incoming.gains_at(midpoint),
            EqGains {
                low: 1.0,
                mid: 1.0,
                high: 1.0,
            }
        );
        assert_eq!(
            outgoing.gains_at(start + duration),
            EqGains {
                low: 0.0,
                mid: 0.7,
                high: 0.8,
            }
        );
        assert_eq!(
            incoming.gains_at(start + duration),
            EqGains {
                low: 1.0,
                mid: 1.0,
                high: 1.0,
            }
        );
    }

    #[test]
    fn equalizer_transition_is_clamped_and_zero_duration_is_safe() {
        for role in [EqTransitionRole::Outgoing, EqTransitionRole::Incoming] {
            let transition = EqTransition {
                id: 7,
                source_start: Duration::from_secs(5),
                duration: Duration::from_secs(4),
                role,
            };
            for position in [
                Duration::ZERO,
                Duration::from_secs(5),
                Duration::from_secs(6),
                Duration::from_secs(7),
                Duration::from_secs(9),
                Duration::MAX,
            ] {
                let gains = transition.gains_at(position);
                assert!((0.0..=1.0).contains(&gains.low));
                assert!((0.0..=1.0).contains(&gains.mid));
                assert!((0.0..=1.0).contains(&gains.high));
                assert!(gains.low.is_finite());
            }

            let zero_duration = EqTransition {
                duration: Duration::ZERO,
                ..transition
            };
            let before = zero_duration.gains_at(Duration::from_secs(4));
            let gains = zero_duration.gains_at(zero_duration.source_start);
            assert_eq!(
                before.low,
                match role {
                    EqTransitionRole::Outgoing => 1.0,
                    EqTransitionRole::Incoming => 0.0,
                }
            );
            assert_eq!(
                gains.low,
                match role {
                    EqTransitionRole::Outgoing => 0.0,
                    EqTransitionRole::Incoming => 1.0,
                }
            );
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
            intro_end: None,
            intro_confidence: 0.0,
            outro_start: None,
            outro_confidence: 0.0,
            vocal_activity: vec![0; 180 * 4],
            vocal_activity_confidences: vec![255; 180 * 4],
            vocal_activity_rate: 4,
            energy_profile: vec![192; 180 * 4],
            energy_profile_rate: 4,
            bpm: Some(bpm),
            beat_confidence: 0.9,
            first_beat: Some(Duration::from_secs(1)),
            beat_markers,
            beat_marker_confidences: Vec::new(),
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
        assert_eq!(
            plan.outgoing_start + plan.duration,
            Duration::from_secs(179)
        );
    }

    #[test]
    fn chooses_beatmatch_for_compatible_confident_tempos() {
        let plan = plan_transition(&analyzed(120.0), &analyzed(124.0), &config());
        assert_eq!(plan.kind, TransitionKind::BeatMatched);
        assert!((plan.incoming_tempo_ratio - 120.0 / 124.0).abs() < 0.0001);
        assert_eq!(plan.duration, Duration::from_secs(10));
    }

    #[test]
    fn beatmatch_accepts_double_time_bpm_family() {
        let outgoing = analyzed(70.0);
        let incoming = analyzed(140.0);
        let plan = plan_transition(&outgoing, &incoming, &config());
        let report = evaluate_transition_quality(&outgoing, &incoming, &plan);

        assert_eq!(plan.kind, TransitionKind::BeatMatched);
        assert!((plan.incoming_tempo_ratio - 1.0).abs() < 0.0001);
        assert!(report.is_ok(), "plan={plan:?} report={report:?}");
    }

    #[test]
    fn kick_marker_coverage_allows_beatmatch_below_global_confidence_threshold() {
        let outgoing = marker_backed_low_confidence(126.0);
        let incoming = marker_backed_low_confidence(126.0);

        let plan = plan_transition(&outgoing, &incoming, &config());
        let report = evaluate_transition_quality(&outgoing, &incoming, &plan);
        let guarded = plan_guarded_transition(&outgoing, &incoming, &config());

        assert_eq!(plan.kind, TransitionKind::BeatMatched);
        assert_eq!(
            explain_beatmatch_decision(&outgoing, &incoming, &config(), &guarded),
            AutoMixBeatMatchDecision::Selected
        );
        assert!(
            report.max_beat_phase_error.unwrap() <= Duration::from_millis(1),
            "plan={plan:?} report={report:?}"
        );
        assert!(!report.has_blocking_issue(), "report={report:?}");
    }

    #[test]
    fn weak_kick_marker_coverage_still_falls_back_to_crossfade() {
        let mut outgoing = marker_backed_low_confidence(126.0);
        let mut incoming = marker_backed_low_confidence(126.0);
        outgoing.beat_marker_confidences.fill(0.1);
        incoming.beat_marker_confidences.fill(0.1);

        let plan = plan_transition(&outgoing, &incoming, &config());
        let guarded = plan_guarded_transition(&outgoing, &incoming, &config());

        assert_eq!(plan.kind, TransitionKind::Crossfade);
        assert_eq!(
            explain_beatmatch_decision(&outgoing, &incoming, &config(), &guarded),
            AutoMixBeatMatchDecision::OutgoingTempoConfidenceTooLow
        );
    }

    #[test]
    fn transition_quality_accepts_a_continuous_beatmatched_mix() {
        let outgoing = analyzed(120.0);
        let incoming = analyzed(120.0);
        let plan = plan_transition(&outgoing, &incoming, &config());

        let report = evaluate_transition_quality(&outgoing, &incoming, &plan);

        assert!(report.is_ok(), "report={report:?} plan={plan:?}");
        assert!(report.beat_pairs_checked >= 8, "report={report:?}");
        assert!(
            report.max_beat_phase_error.unwrap() <= Duration::from_millis(1),
            "report={report:?}"
        );
        assert!(
            report.handoff_beat_phase_error.unwrap() <= Duration::from_millis(1),
            "report={report:?}"
        );
        assert!(report.downbeat_pairs_checked >= 2, "report={report:?}");
        assert!(
            report.max_downbeat_phase_error.unwrap() <= Duration::from_millis(1),
            "report={report:?}"
        );
        assert!(
            report.handoff_downbeat_phase_error.unwrap() <= Duration::from_millis(1),
            "report={report:?}"
        );
        assert!(report.phrase_pairs_checked >= 2, "report={report:?}");
        assert!(
            report.max_phrase_phase_error.unwrap() <= Duration::from_millis(1),
            "report={report:?}"
        );
        assert!(
            report.handoff_phrase_phase_error.unwrap() <= Duration::from_millis(1),
            "report={report:?}"
        );
        assert!(
            report.low_handoff_min.unwrap() >= 0.99 && report.low_handoff_max.unwrap() <= 1.01,
            "report={report:?}"
        );
        assert!(report.energy_samples_checked > 0, "report={report:?}");
        assert!(
            report.min_mix_energy_ratio.unwrap() >= 0.75,
            "report={report:?}"
        );
        assert!(
            report.max_mix_energy_ratio.unwrap() <= 1.05,
            "report={report:?}"
        );
    }

    #[test]
    fn transition_quality_detects_late_incoming_kicks() {
        let outgoing = analyzed(120.0);
        let incoming = analyzed(120.0);
        let mut plan = plan_transition(&outgoing, &incoming, &config());
        plan.incoming_start += Duration::from_millis(75);

        let report = evaluate_transition_quality(&outgoing, &incoming, &plan);

        assert!(
            report.issues.iter().any(|issue| matches!(
                issue,
                AutoMixQualityIssue::BeatPhaseDriftTooLarge { max_error }
                    if *max_error > Duration::from_millis(35)
            )),
            "report={report:?}"
        );
    }

    #[test]
    fn transition_quality_detects_handoff_phase_drift() {
        let outgoing = analyzed(120.0);
        let incoming = analyzed(120.0);
        let mut plan = plan_transition(&outgoing, &incoming, &config());
        plan.incoming_tempo_ratio = 1.02;
        plan.tempo_envelope = Some(TempoEnvelope::new(
            1.02,
            1.02,
            plan.duration,
            Duration::ZERO,
        ));

        let report = evaluate_transition_quality(&outgoing, &incoming, &plan);

        assert!(
            report.handoff_beat_phase_error.unwrap() > Duration::from_millis(35),
            "report={report:?}"
        );
        assert!(
            report.issues.iter().any(|issue| matches!(
                issue,
                AutoMixQualityIssue::BeatHandoffPhaseDriftTooLarge { error }
                    if *error > Duration::from_millis(35)
            )),
            "report={report:?}"
        );
        assert!(report.has_blocking_issue(), "report={report:?}");
    }

    #[test]
    fn transition_quality_detects_downbeat_phase_drift() {
        let outgoing = analyzed(120.0);
        let mut incoming = analyzed(120.0);
        let plan = plan_transition(&outgoing, &incoming, &config());
        incoming.first_downbeat = Some(Duration::from_millis(1_500));
        incoming.downbeat_confidence = 0.9;

        let report = evaluate_transition_quality(&outgoing, &incoming, &plan);

        assert_eq!(plan.kind, TransitionKind::BeatMatched);
        assert!(
            report.max_beat_phase_error.unwrap() <= Duration::from_millis(1),
            "report={report:?}"
        );
        assert!(
            report.issues.iter().any(|issue| matches!(
                issue,
                AutoMixQualityIssue::DownbeatPhaseDriftTooLarge { max_error }
                    if *max_error > Duration::from_millis(400)
            )),
            "report={report:?}"
        );
        assert!(report.has_blocking_issue(), "report={report:?}");
    }

    #[test]
    fn transition_quality_detects_phrase_phase_drift() {
        let outgoing = analyzed(120.0);
        let mut incoming = analyzed(120.0);
        let plan = plan_transition(&outgoing, &incoming, &config());
        incoming.first_downbeat = Some(Duration::from_secs(3));
        incoming.downbeat_confidence = 0.9;

        let report = evaluate_transition_quality(&outgoing, &incoming, &plan);

        assert_eq!(plan.kind, TransitionKind::BeatMatched);
        assert!(
            report.max_downbeat_phase_error.unwrap() <= Duration::from_millis(1),
            "report={report:?}"
        );
        assert!(
            report.issues.iter().any(|issue| matches!(
                issue,
                AutoMixQualityIssue::PhrasePhaseDriftTooLarge { max_error }
                    if *max_error > Duration::from_millis(1_900)
            )),
            "report={report:?}"
        );
        assert!(report.has_blocking_issue(), "report={report:?}");
    }

    #[test]
    fn beatmatch_aligns_outgoing_start_to_incoming_phrase_phase() {
        let outgoing = analyzed(120.0);
        let mut incoming = analyzed(120.0);
        incoming.first_downbeat = Some(Duration::from_millis(1_500));
        incoming.downbeat_confidence = 0.9;

        let plan = plan_transition(&outgoing, &incoming, &config());
        let quality = evaluate_transition_quality(&outgoing, &incoming, &plan);

        assert_eq!(plan.kind, TransitionKind::BeatMatched);
        assert_eq!(plan.incoming_start, Duration::from_secs(1));
        assert_eq!(plan.outgoing_start, Duration::from_millis(168_500));
        assert!(
            quality.max_downbeat_phase_error.unwrap() <= Duration::from_millis(1),
            "plan={plan:?} report={quality:?}"
        );
        assert!(
            quality.max_phrase_phase_error.unwrap() <= Duration::from_millis(1),
            "plan={plan:?} report={quality:?}"
        );
        assert!(!quality.has_blocking_issue(), "report={quality:?}");
    }

    #[test]
    fn beatmatch_aligns_longer_pickup_to_incoming_phrase_phase() {
        let outgoing = analyzed(120.0);
        let mut incoming = analyzed(120.0);
        incoming.first_downbeat = Some(Duration::from_secs(3));
        incoming.downbeat_confidence = 0.9;

        let plan = plan_transition(&outgoing, &incoming, &config());
        let quality = evaluate_transition_quality(&outgoing, &incoming, &plan);

        assert_eq!(plan.kind, TransitionKind::BeatMatched);
        assert_eq!(plan.incoming_start, Duration::from_secs(1));
        assert_eq!(plan.outgoing_start, Duration::from_secs(167));
        assert!(
            quality.max_downbeat_phase_error.unwrap() <= Duration::from_millis(1),
            "plan={plan:?} report={quality:?}"
        );
        assert!(
            quality.max_phrase_phase_error.unwrap() <= Duration::from_millis(1),
            "plan={plan:?} report={quality:?}"
        );
        assert!(!quality.has_blocking_issue(), "report={quality:?}");
    }

    #[test]
    fn transition_quality_detects_a_stopped_flow_transition() {
        let outgoing = analyzed(120.0);
        let incoming = analyzed(120.0);
        let mut plan = plan_transition(&outgoing, &incoming, &config());
        plan.duration = Duration::ZERO;
        plan.outgoing_start = outgoing.audible_end;

        let report = evaluate_transition_quality(&outgoing, &incoming, &plan);

        assert!(
            report
                .issues
                .contains(&AutoMixQualityIssue::MixOverlapTooShort {
                    overlap: Duration::ZERO
                }),
            "report={report:?}"
        );
        assert!(
            report
                .issues
                .contains(&AutoMixQualityIssue::BeatPhaseUnverified),
            "report={report:?}"
        );
    }

    #[test]
    fn incoming_start_uses_trusted_kick_marker_after_the_audible_boundary() {
        let outgoing = analyzed(120.0);
        let mut incoming = analyzed(120.0);
        incoming.first_beat = Some(Duration::from_secs(1));
        incoming.first_downbeat = Some(Duration::from_millis(1_050));
        incoming.beat_markers = std::iter::once(Duration::from_millis(980))
            .chain((0..350).map(|index| Duration::from_millis(1_050 + index * 500)))
            .collect();
        incoming.beat_marker_confidences = vec![1.0; incoming.beat_markers.len()];

        let plan = plan_transition(&outgoing, &incoming, &config());
        let quality = evaluate_transition_quality(&outgoing, &incoming, &plan);
        let guarded = plan_guarded_transition(&outgoing, &incoming, &config());

        assert_eq!(plan.kind, TransitionKind::BeatMatched);
        assert_eq!(plan.incoming_start, Duration::from_millis(1_050));
        assert!(!quality.has_blocking_issue(), "report={quality:?}");
        assert!(guarded.rejected_plan.is_none(), "guarded={guarded:?}");
    }

    #[test]
    fn structured_overlap_stays_inside_outro_while_preserving_phrase_phase() {
        let mut outgoing = analyzed(120.0);
        outgoing.audible_end = Duration::from_secs(61);
        outgoing.outro_start = Some(Duration::from_secs(50));
        outgoing.outro_confidence = 0.9;
        let mut incoming = analyzed(120.0);
        incoming.audible_end = Duration::from_secs(61);
        incoming.intro_end = Some(Duration::from_secs(13));
        incoming.intro_confidence = 0.9;
        let mut config = config();
        config.crossfade = Duration::from_secs(16);

        let plan = plan_transition(&outgoing, &incoming, &config);
        let quality = evaluate_transition_quality(&outgoing, &incoming, &plan);

        assert_eq!(plan.kind, TransitionKind::BeatMatched);
        assert_eq!(plan.outgoing_start, Duration::from_secs(57));
        assert!(plan.outgoing_start >= outgoing.outro_start.unwrap());
        assert_eq!(plan.outgoing_start + plan.duration, outgoing.audible_end);
        assert!(
            quality.max_downbeat_phase_error.unwrap() <= Duration::from_millis(1),
            "plan={plan:?} report={quality:?}"
        );
        assert!(
            quality.max_phrase_phase_error.unwrap() <= Duration::from_millis(1),
            "plan={plan:?} report={quality:?}"
        );
        assert!(!quality.has_blocking_issue(), "report={quality:?}");
    }

    #[test]
    fn guarded_transition_replaces_a_drifted_beatmatch_with_conservative_crossfade() {
        let outgoing = analyzed(120.0);
        let mut incoming = analyzed(120.0);
        for marker in &mut incoming.beat_markers {
            *marker += Duration::from_millis(250);
        }

        let raw_plan = plan_transition(&outgoing, &incoming, &config());
        let raw_quality = evaluate_transition_quality(&outgoing, &incoming, &raw_plan);

        assert_eq!(raw_plan.kind, TransitionKind::BeatMatched);
        assert!(raw_quality.has_blocking_issue(), "report={raw_quality:?}");

        let guarded = plan_guarded_transition(&outgoing, &incoming, &config());

        assert_eq!(
            guarded.rejected_plan.as_ref().map(|plan| plan.kind),
            Some(TransitionKind::BeatMatched)
        );
        assert!(
            guarded
                .rejected_quality
                .as_ref()
                .is_some_and(AutoMixQualityReport::has_blocking_issue),
            "guarded={guarded:?}"
        );
        assert_eq!(guarded.plan.kind, TransitionKind::Crossfade);
        assert_eq!(guarded.plan.incoming_start, incoming.audible_start);
        assert_eq!(
            guarded.plan.outgoing_start + guarded.plan.duration,
            outgoing.audible_end
        );
        assert_eq!(guarded.plan.incoming_tempo_ratio, 1.0);
        assert!(guarded.plan.tempo_envelope.is_none());
        assert!(!guarded.quality.has_blocking_issue(), "guarded={guarded:?}");
        assert_eq!(
            explain_beatmatch_decision(&outgoing, &incoming, &config(), &guarded),
            AutoMixBeatMatchDecision::QualityGuarded
        );
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
        let outgoing = analyzed(90.0);
        let incoming = analyzed(140.0);
        let plan = plan_transition(&outgoing, &incoming, &config());
        let guarded = plan_guarded_transition(&outgoing, &incoming, &config());
        assert_eq!(plan.kind, TransitionKind::Crossfade);
        assert_eq!(plan.incoming_tempo_ratio, 1.0);
        assert_eq!(
            explain_beatmatch_decision(&outgoing, &incoming, &config(), &guarded),
            AutoMixBeatMatchDecision::TempoDifferenceTooLarge
        );
    }

    #[test]
    fn recognizes_relative_keys_as_harmonically_compatible() {
        let plan = plan_transition(
            &keyed(0, KeyMode::Major),
            &keyed(9, KeyMode::Minor),
            &config(),
        );
        assert_eq!(plan.harmonic_compatibility, Some(0.95));
        assert_eq!(plan.duration, Duration::from_secs(10));
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
    fn limits_incoming_gain_to_keep_equal_power_mix_below_peak_headroom() {
        let mut outgoing = analyzed(120.0);
        outgoing.rms_dbfs = Some(-12.0);
        outgoing.sample_peak_dbfs = Some(0.0);
        let mut incoming = analyzed(120.0);
        incoming.rms_dbfs = Some(-12.0);
        incoming.sample_peak_dbfs = Some(0.0);

        let plan = plan_transition(&outgoing, &incoming, &config());

        assert!((0.25..0.31).contains(&plan.incoming_gain), "plan={plan:?}");
        let midpoint_peak = std::f32::consts::FRAC_1_SQRT_2
            * (dbfs_to_linear(outgoing.sample_peak_dbfs.unwrap())
                + dbfs_to_linear(incoming.sample_peak_dbfs.unwrap()) * plan.incoming_gain);
        assert!(
            midpoint_peak <= dbfs_to_linear(MIX_PEAK_HEADROOM_DBFS) + 0.001,
            "midpoint_peak={midpoint_peak} plan={plan:?}"
        );
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
    fn phase_follow_uses_global_tempo_for_low_confidence_block() {
        let mut outgoing = analyzed(120.0);
        let mut incoming = analyzed(120.0);
        outgoing.beat_markers = (0..=16)
            .map(|index| Duration::from_secs_f32(1.0 + index as f32 * 0.5))
            .collect();
        incoming.beat_markers.clear();
        let mut beat = Duration::from_secs(1);
        incoming.beat_markers.push(beat);
        for index in 0..16 {
            beat += Duration::from_secs_f32(if (4..8).contains(&index) { 0.48 } else { 0.5 });
            incoming.beat_markers.push(beat);
        }
        outgoing.beat_marker_confidences = vec![1.0; outgoing.beat_markers.len()];
        incoming.beat_marker_confidences = vec![1.0; incoming.beat_markers.len()];
        incoming.beat_marker_confidences[4] = 0.0;
        incoming.beat_marker_confidences[8] = 0.0;

        let segments = phase_follow_segments(
            &outgoing,
            &incoming,
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(8),
            1.0,
            0.06,
        )
        .unwrap();

        assert!((segments[0].speed - 1.0).abs() < 0.0001);
        assert!((segments[1].speed - 1.0).abs() < 0.0001);
        assert!((segments[2].speed - 1.0).abs() < 0.0001);
        assert!(segments[3].speed < 0.98);
    }

    #[test]
    fn distant_trusted_marker_does_not_override_global_grid() {
        let mut analysis = analyzed(120.0);
        analysis.beat_marker_confidences = vec![0.0; analysis.beat_markers.len()];
        let distant = analysis
            .beat_markers
            .iter()
            .position(|beat| *beat == Duration::from_secs(150))
            .unwrap();
        analysis.beat_marker_confidences[distant] = 1.0;

        assert_eq!(
            align_to_beat(Duration::from_secs(171), &analysis),
            Duration::from_secs(171)
        );
        assert_eq!(
            snap_to_nearest_beat(&analysis, Duration::from_secs(1)),
            None
        );
    }

    #[test]
    fn trusted_intro_and_outro_choose_track_specific_overlap() {
        let mut outgoing = analyzed(120.0);
        outgoing.audible_end = Duration::from_secs(61);
        outgoing.outro_start = Some(Duration::from_secs(49));
        outgoing.outro_confidence = 0.9;
        let mut incoming = analyzed(120.0);
        incoming.audible_end = Duration::from_secs(61);
        incoming.intro_end = Some(Duration::from_secs(13));
        incoming.intro_confidence = 0.9;
        let mut config = config();
        config.crossfade = Duration::from_secs(16);

        let plan = plan_transition(&outgoing, &incoming, &config);

        assert_eq!(plan.kind, TransitionKind::BeatMatched);
        assert_eq!(plan.outgoing_start, Duration::from_secs(49));
        assert_eq!(plan.duration, Duration::from_secs(12));
        assert_eq!(plan.outgoing_start + plan.duration, outgoing.audible_end);
        assert_eq!(plan.incoming_start, incoming.audible_start);
    }

    #[test]
    fn audible_pickup_before_first_beat_is_not_trimmed() {
        let outgoing = analyzed(120.0);
        let mut incoming = analyzed(120.0);
        incoming.first_beat = Some(Duration::from_millis(1_250));
        incoming.first_downbeat = Some(Duration::from_millis(1_250));
        incoming.beat_markers = (0..350)
            .map(|index| Duration::from_secs_f32(1.25 + index as f32 * 0.5))
            .collect();

        let plan = plan_transition(&outgoing, &incoming, &config());

        assert_eq!(plan.kind, TransitionKind::Crossfade);
        assert_eq!(plan.incoming_start, incoming.audible_start);
        assert_eq!(plan.outgoing_start + plan.duration, outgoing.audible_end);
    }

    #[test]
    fn vocal_edges_shorten_overlap_to_avoid_dual_vocals() {
        let mut outgoing = analyzed(120.0);
        outgoing.audible_end = Duration::from_secs(61);
        outgoing.outro_start = Some(Duration::from_secs(49));
        outgoing.outro_confidence = 0.9;
        set_vocal_ranges(&mut outgoing, &[(1.0, 57.0)]);
        let mut incoming = analyzed(120.0);
        incoming.audible_end = Duration::from_secs(61);
        incoming.intro_end = Some(Duration::from_secs(13));
        incoming.intro_confidence = 0.9;
        set_vocal_ranges(&mut incoming, &[(3.0, 50.0)]);
        let mut config = config();
        config.crossfade = Duration::from_secs(16);

        let plan = plan_transition(&outgoing, &incoming, &config);
        let quality = evaluate_transition_quality(&outgoing, &incoming, &plan);

        assert_eq!(plan.duration, Duration::from_secs(4), "plan={plan:?}");
        assert_eq!(plan.outgoing_start, Duration::from_secs(57));
        assert_eq!(plan.outgoing_start + plan.duration, outgoing.audible_end);
        assert!(
            quality.max_phrase_phase_error.unwrap() <= Duration::from_millis(1),
            "plan={plan:?} report={quality:?}"
        );
        assert!(quality.vocal_overlap_samples_checked > 0);
        assert!(
            quality.max_dual_vocal_risk.unwrap() <= MAX_DUAL_VOCAL_RISK,
            "plan={plan:?} report={quality:?}"
        );
        assert!(!quality.has_blocking_issue(), "report={quality:?}");
    }

    #[test]
    fn transition_quality_detects_dual_vocal_overlap() {
        let mut outgoing = analyzed(120.0);
        outgoing.audible_end = Duration::from_secs(61);
        set_vocal_ranges(&mut outgoing, &[(53.0, 61.0)]);
        let mut incoming = analyzed(120.0);
        incoming.audible_end = Duration::from_secs(61);
        set_vocal_ranges(&mut incoming, &[(1.0, 9.0)]);
        let plan = TransitionPlan {
            kind: TransitionKind::Crossfade,
            outgoing_start: Duration::from_secs(53),
            incoming_start: Duration::from_secs(1),
            duration: Duration::from_secs(8),
            incoming_tempo_ratio: 1.0,
            harmonic_compatibility: None,
            incoming_gain: 1.0,
            tempo_envelope: None,
            energy_selection: None,
        };

        let report = evaluate_transition_quality(&outgoing, &incoming, &plan);

        assert!(report.vocal_overlap_samples_checked > 0);
        assert!(
            report.max_dual_vocal_risk.unwrap() > MAX_DUAL_VOCAL_RISK,
            "report={report:?}"
        );
        assert!(
            report.issues.iter().any(|issue| matches!(
                issue,
                AutoMixQualityIssue::DualVocalOverlapTooHigh { max_risk }
                    if *max_risk > MAX_DUAL_VOCAL_RISK
            )),
            "report={report:?}"
        );
        assert!(report.has_blocking_issue(), "report={report:?}");
    }

    #[test]
    fn transition_quality_reports_a_mid_mix_energy_dip() {
        let mut outgoing = analyzed(120.0);
        let mut incoming = analyzed(120.0);
        set_energy_ranges(&mut outgoing, -18.0, &[(175.0, 177.0, -42.0)]);
        set_energy_ranges(&mut incoming, -18.0, &[(5.0, 7.0, -42.0)]);
        let plan = TransitionPlan {
            kind: TransitionKind::Crossfade,
            outgoing_start: Duration::from_secs(171),
            incoming_start: Duration::from_secs(1),
            duration: Duration::from_secs(8),
            incoming_tempo_ratio: 1.0,
            harmonic_compatibility: None,
            incoming_gain: 1.0,
            tempo_envelope: None,
            energy_selection: None,
        };

        let report = evaluate_transition_quality(&outgoing, &incoming, &plan);

        assert!(report.energy_samples_checked > 0, "report={report:?}");
        assert!(
            report.min_mix_energy_ratio.unwrap() < 0.2,
            "report={report:?}"
        );
    }

    #[test]
    fn energy_balancing_prefers_clean_phrase_boundary_over_default_dip() {
        let mut config = config();
        config.crossfade = Duration::from_secs(16);
        let mut outgoing = analyzed(120.0);
        let mut incoming = analyzed(120.0);
        set_energy_ranges(&mut outgoing, -18.0, &[(168.0, 170.0, -42.0)]);
        set_energy_ranges(&mut incoming, -18.0, &[(8.0, 10.0, -42.0)]);

        let plan = plan_transition(&outgoing, &incoming, &config);
        let report = evaluate_transition_quality(&outgoing, &incoming, &plan);

        assert_eq!(plan.kind, TransitionKind::BeatMatched);
        assert_eq!(plan.incoming_start, Duration::from_secs(1));
        assert_eq!(plan.outgoing_start, Duration::from_secs(153));
        let energy_selection = plan.energy_selection.expect("energy selection");
        assert_eq!(energy_selection.default_start, Duration::from_secs(161));
        assert_eq!(energy_selection.selected_start, plan.outgoing_start);
        assert!(energy_selection.candidates_checked > 1);
        assert!(
            report.min_mix_energy_ratio.unwrap() > 0.6,
            "plan={plan:?} report={report:?}"
        );
        assert!(!report.has_blocking_issue(), "report={report:?}");
    }

    #[test]
    fn energy_balancing_improves_crossfade_when_beatmatch_is_unavailable() {
        let mut outgoing = analyzed(90.0);
        let mut incoming = analyzed(140.0);
        set_energy_ranges(&mut outgoing, -18.0, &[(175.0, 177.0, -42.0)]);
        set_energy_ranges(&mut incoming, -18.0, &[(5.0, 7.0, -42.0)]);

        let plan = plan_transition(&outgoing, &incoming, &config());
        let report = evaluate_transition_quality(&outgoing, &incoming, &plan);

        assert_eq!(plan.kind, TransitionKind::Crossfade);
        assert!(
            plan.outgoing_start < Duration::from_secs(171),
            "plan={plan:?}"
        );
        let energy_selection = plan.energy_selection.expect("energy selection");
        assert_eq!(energy_selection.default_start, Duration::from_secs(171));
        assert_eq!(energy_selection.selected_start, plan.outgoing_start);
        assert!(energy_selection.candidates_checked > 1);
        assert!(
            report.min_mix_energy_ratio.unwrap() > 0.6,
            "plan={plan:?} report={report:?}"
        );
        assert!(!report.has_blocking_issue(), "report={report:?}");
    }

    #[test]
    fn dual_vocal_quality_is_weighted_by_the_actual_fade_gain() {
        let mut outgoing = analyzed(120.0);
        outgoing.audible_end = Duration::from_secs(61);
        set_vocal_ranges(&mut outgoing, &[(53.0, 55.0)]);
        let mut incoming = analyzed(120.0);
        incoming.audible_end = Duration::from_secs(61);
        set_vocal_ranges(&mut incoming, &[(1.0, 3.0)]);
        let plan = TransitionPlan {
            kind: TransitionKind::Crossfade,
            outgoing_start: Duration::from_secs(53),
            incoming_start: Duration::from_secs(1),
            duration: Duration::from_secs(8),
            incoming_tempo_ratio: 1.0,
            harmonic_compatibility: None,
            incoming_gain: 1.0,
            tempo_envelope: None,
            energy_selection: None,
        };

        let report = evaluate_transition_quality(&outgoing, &incoming, &plan);

        assert!(report.vocal_overlap_samples_checked > 0);
        assert!(
            report.max_dual_vocal_risk.unwrap() < MAX_DUAL_VOCAL_RISK,
            "report={report:?}"
        );
        assert!(
            !report
                .issues
                .iter()
                .any(|issue| matches!(issue, AutoMixQualityIssue::DualVocalOverlapTooHigh { .. })),
            "report={report:?}"
        );
    }

    #[test]
    fn vocals_at_both_transition_edges_fall_back_to_gapless() {
        let mut outgoing = analyzed(120.0);
        set_vocal_ranges(&mut outgoing, &[(1.0, 179.0)]);
        let mut incoming = analyzed(120.0);
        set_vocal_ranges(&mut incoming, &[(1.0, 179.0)]);

        let plan = plan_transition(&outgoing, &incoming, &config());

        assert_eq!(plan.kind, TransitionKind::Gapless);
        assert_eq!(plan.duration, Duration::ZERO);
    }

    #[test]
    fn vocal_limit_uses_tempo_mapped_incoming_time() {
        let mut outgoing = analyzed(120.0);
        outgoing.audible_end = Duration::from_secs(61);
        set_vocal_ranges(&mut outgoing, &[(1.0, 57.0)]);
        let mut incoming = analyzed(124.0);
        incoming.audible_end = Duration::from_secs(61);
        set_vocal_ranges(&mut incoming, &[(4.0, 50.0)]);
        let mut config = config();
        config.crossfade = Duration::from_secs(16);

        let plan = plan_transition(&outgoing, &incoming, &config);
        let envelope = plan.tempo_envelope.expect("tempo mapping should be active");
        let safe = Duration::from_secs(4) + envelope.output_elapsed(Duration::from_secs(3));

        assert!(plan.duration <= safe, "plan={plan:?} safe={safe:?}");
        assert_eq!(plan.outgoing_start + plan.duration, outgoing.audible_end);
    }

    #[test]
    fn vocal_pickup_inside_trim_window_disables_beatmatch() {
        let outgoing = analyzed(120.0);
        let mut incoming = analyzed(120.0);
        incoming.first_beat = Some(Duration::from_millis(1_050));
        incoming.beat_markers[0] = Duration::from_millis(1_050);
        set_vocal_ranges(&mut incoming, &[(1.0, 2.0)]);

        let plan = plan_transition(&outgoing, &incoming, &config());

        assert_eq!(plan.kind, TransitionKind::Crossfade);
        assert_eq!(plan.incoming_start, incoming.audible_start);
    }

    #[test]
    fn vocal_summary_keeps_a_bin_crossing_the_audible_boundary() {
        let mut analysis = analyzed(120.0);
        analysis.audible_start = Duration::from_millis(1_125);
        set_vocal_ranges(&mut analysis, &[(1.0, 2.0)]);

        let summary = summarize_vocals(&analysis);

        assert!(summary.known);
        assert_eq!(summary.first, Some(analysis.audible_start));
        assert_eq!(summary.last_end, Some(Duration::from_secs(2)));
    }

    #[test]
    fn every_planned_overlap_respects_its_final_vocal_time_map() {
        for bpm in [116.0, 120.0, 124.0] {
            for incoming_vocal in [2.0, 3.0, 4.0] {
                let mut outgoing = analyzed(120.0);
                outgoing.audible_end = Duration::from_secs(61);
                set_vocal_ranges(&mut outgoing, &[(1.0, 57.0)]);
                let mut incoming = analyzed(bpm);
                incoming.audible_end = Duration::from_secs(61);
                set_vocal_ranges(&mut incoming, &[(incoming_vocal, 50.0)]);
                let mut config = config();
                config.crossfade = Duration::from_secs(16);

                let plan = plan_transition(&outgoing, &incoming, &config);
                let safe = vocal_overlap_limit(
                    &outgoing,
                    &incoming,
                    plan.incoming_start,
                    plan.duration,
                    plan.tempo_envelope,
                );

                assert!(
                    plan.duration <= safe,
                    "bpm={bpm} plan={plan:?} safe={safe:?}"
                );
            }
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

    fn set_vocal_ranges(analysis: &mut TrackAnalysis, ranges: &[(f32, f32)]) {
        let rate = 4_usize;
        let length = (analysis.duration.as_secs_f32() * rate as f32).ceil() as usize;
        analysis.vocal_activity = vec![0; length];
        analysis.vocal_activity_confidences = vec![255; length];
        analysis.vocal_activity_rate = rate as u8;
        for (start, end) in ranges {
            let from = (*start * rate as f32).floor() as usize;
            let to = (*end * rate as f32).ceil() as usize;
            analysis.vocal_activity[from..to.min(length)].fill(255);
        }
    }

    fn marker_backed_low_confidence(bpm: f32) -> TrackAnalysis {
        let mut analysis = analyzed(bpm);
        analysis.beat_confidence = 0.56;
        analysis.beat_marker_confidences = vec![0.75; analysis.beat_markers.len()];
        analysis
    }

    fn set_energy_ranges(analysis: &mut TrackAnalysis, base_dbfs: f32, ranges: &[(f32, f32, f32)]) {
        let rate = 4_usize;
        let length = (analysis.duration.as_secs_f32() * rate as f32).ceil() as usize;
        analysis.energy_profile = vec![energy_code(base_dbfs); length];
        analysis.energy_profile_rate = rate as u8;
        for (start, end, dbfs) in ranges {
            let from = (*start * rate as f32).floor() as usize;
            let to = (*end * rate as f32).ceil() as usize;
            analysis.energy_profile[from..to.min(length)].fill(energy_code(*dbfs));
        }
    }

    fn energy_code(dbfs: f32) -> u8 {
        (((dbfs.clamp(MIN_ENERGY_PROFILE_DBFS, 0.0) - MIN_ENERGY_PROFILE_DBFS)
            / -MIN_ENERGY_PROFILE_DBFS)
            * 255.0)
            .round() as u8
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
