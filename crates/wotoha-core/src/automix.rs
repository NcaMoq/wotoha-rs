use std::time::Duration;

use crate::vocal_analysis::effective_vocal_risk;

/// The V2 planner is intentionally additive.  The original planner in this
/// file is still used by the playback/runtime crates; these modules provide a
/// timeline-first API that can be adopted by callers independently.
pub mod candidate;
pub mod diagnostics;
pub mod reliability;
pub mod scoring;

pub use crate::analysis::BeatEvent;
pub use candidate::{
    BeatMatchEligibility, BeatMatchRejection, GuardedTransitionPlanV2, TempoHypothesis,
    TempoHypothesisPair, TransitionCandidate, TransitionPlanV2, V2AnalysisInput,
    V2GuardedTransitionPlan, V2TransitionPlan, beat_match_eligibility,
    beat_match_eligibility_for_timelines, check_beat_match_eligibility,
    cross_product_tempo_hypotheses, explain_beatmatch_decision_v2, plan_guarded_transition_v2,
    plan_guarded_transition_v2_diagnostics, plan_guarded_transition_v2_for_analysis,
    plan_guarded_transition_v2_with_diagnostics, plan_transition_v2,
    plan_transition_v2_diagnostics, plan_transition_v2_for_analysis,
    plan_transition_v2_with_diagnostics, select_tempo_hypothesis_pair, tempo_hypotheses,
    tempo_hypothesis_pairs,
};
pub use diagnostics::{AutoMixV2Reason, CandidateRejection, PlannerDiagnostics};
pub use reliability::{
    BeatTimeline, ReliabilityBreakdown, TimelineEvent, compute_pair_reliability,
    compute_pair_reliability_for_timelines, compute_pair_reliability_v2, compute_reliability,
    compute_reliability_v2, geometric_pair_reliability, pair_reliability, reliability,
    reliability_for_beat_events, reliability_for_rhythm, reliability_for_timeline,
    reliability_for_timeline_window, reliability_for_track_analysis_v2, timeline_from_analysis,
    timeline_from_beat_events, timeline_from_rhythm, timeline_from_track_analysis_v2,
};
pub use scoring::TransitionCostBreakdown;

pub const TEMPO_SYNC_DEADBAND: f32 = 0.001;
const MAX_TEMPO_SEGMENTS: usize = 32;
const MIN_PHASE_MARKER_CONFIDENCE: f32 = 0.35;
const MIN_AUDIBLE_MIX_OVERLAP: Duration = Duration::from_secs(1);
pub(crate) const MAX_BEATMATCH_PHASE_ERROR: Duration = Duration::from_millis(35);
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
const MAX_INCOMING_PICKUP_BEATS: u32 = 32;
const MAX_LOW_ENERGY_INTRO_SKIP: Duration = Duration::from_secs(8);
const MAX_STRUCTURED_INTRO_SKIP: Duration = Duration::from_secs(16);
const MAX_SKIPPED_INTRO_VOCAL_RISK: f32 = 0.2;
const MIN_ACTIONABLE_DOWNBEAT_CONFIDENCE: f32 = 0.6;
const MAX_SHORTER_TRANSITION_SEARCH: Duration = Duration::from_secs(8);
const MIN_NATURAL_MIX_OVERLAP: Duration = Duration::from_secs(4);
const MIN_ENERGY_PROFILE_DBFS: f32 = -80.0;
/// The analysis profile is an instantaneous, band-averaged proxy and reads
/// materially lower than the preview's windowed RMS for healthy transitions.
/// Reserve blocking for a roughly 14 dB analysis-profile collapse; shallower
/// movement remains a scoring concern and is verified by the rendered preview.
const MIN_SAFE_MIX_ENERGY_RATIO: f32 = 0.20;
const MIN_BLOCKING_ENERGY_GAP: Duration = Duration::from_millis(500);
const MIN_ENERGY_WINDOW: Duration = Duration::from_millis(500);
const ENERGY_REFERENCE_RADIUS: Duration = Duration::from_secs(2);
const MAX_ENERGY_REFERENCE_SAMPLES: usize = 64;
const MAX_PHASE_MARKER_PAIRS: usize = 128;
const MIN_BEAT_PHASE_PAIRS: usize = 8;
const MIN_BEAT_PHASE_COVERAGE: f32 = 0.65;
const MIN_DOWNBEAT_PHASE_PAIRS: usize = 2;
const MIN_PHRASE_PHASE_PAIRS: usize = 1;
// Treat only a very deep, simultaneous source dip as an unavoidable
// structural break. A merely deep (-42 dBFS-style) dip remains actionable
// when a normal source reference is available nearby.
const MIN_STRUCTURAL_BREAK_RATIO: f32 = 0.05;
const MAX_STRUCTURAL_BREAK_DURATION: Duration = Duration::from_secs(4);
const MAX_INCOMING_CUE_CANDIDATES: usize = 96;
const MIN_TRUSTED_BPM: f32 = 20.0;
const MAX_TRUSTED_BPM: f32 = 300.0;

fn beat_interval_from_bpm(bpm: f32) -> Option<Duration> {
    if !bpm.is_finite() || !(MIN_TRUSTED_BPM..=MAX_TRUSTED_BPM).contains(&bpm) {
        return None;
    }

    let seconds = 60.0 / bpm;
    if !seconds.is_finite() || seconds <= 0.0 {
        return None;
    }
    // Keep the grid quantization identical to the decoder/audio-analysis
    // markers, which are derived from the f32 BPM estimate.
    let interval = Duration::from_secs_f32(seconds);
    (!interval.is_zero()).then_some(interval)
}

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
    /// ITU-R BS.1770 integrated programme loudness in LUFS.
    pub integrated_lufs: Option<f32>,
    /// Maximum oversampled true peak in dBTP across all source channels.
    pub true_peak_dbtp: Option<f32>,
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
            integrated_lufs: None,
            true_peak_dbtp: None,
        }
    }

    pub fn beat_grid(&self) -> Option<BeatGrid> {
        let first_downbeat = self.first_downbeat?;
        let bpm = self.bpm?;
        if bpm <= 0.0 || !bpm.is_finite() || self.beat_confidence <= 0.0 {
            return None;
        }
        let beat_interval = beat_interval_from_bpm(bpm)?;
        Some(BeatGrid {
            first_downbeat,
            beat_interval,
            beats_per_bar: 4,
            downbeat_confidence: self.downbeat_confidence,
        })
    }

    pub fn trusted_kick_coverage(&self) -> f32 {
        // Marker confidence is a per-observation field.  A short/missing
        // vector must not inherit the global beat confidence: doing so would
        // turn an untrusted marker cache into a trusted cue source.
        if self.beat_markers.is_empty()
            || self.beat_marker_confidences.len() != self.beat_markers.len()
        {
            return 0.0;
        }
        self.beat_marker_confidences
            .iter()
            .filter(|confidence| {
                let value = **confidence;
                value.is_finite()
                    && (0.0..=1.0).contains(&value)
                    && value >= MIN_PHASE_MARKER_CONFIDENCE
            })
            .count() as f32
            / self.beat_marker_confidences.len() as f32
    }

    /// Returns inferred 4/8/16-bar boundaries inside the audible region.
    pub fn phrase_cues(&self) -> Vec<PhraseCue> {
        let Some(grid) = self.beat_grid() else {
            return Vec::new();
        };
        if grid.downbeat_confidence < MIN_ACTIONABLE_DOWNBEAT_CONFIDENCE {
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

/// Peak measurements used to protect the two-deck mix only when the source
/// peaks make the unmodified transition curve unsafe.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AutoMixPeakGuard {
    /// Outgoing source peak after its already-applied base gain.
    pub outgoing_peak: f32,
    /// Incoming source peak after its retained/base gain.
    pub incoming_peak: f32,
}

impl Default for AutoMixPeakGuard {
    fn default() -> Self {
        Self::unity()
    }
}

impl AutoMixPeakGuard {
    pub const fn new(outgoing_peak: f32, incoming_peak: f32) -> Self {
        Self {
            outgoing_peak,
            incoming_peak,
        }
    }

    /// Conservative guard for analyses that do not contain peak measurements.
    pub const fn unity() -> Self {
        Self {
            outgoing_peak: 1.0,
            incoming_peak: 1.0,
        }
    }

    /// Builds a guard from source dBFS peaks and the gains already applied to
    /// each deck. Missing or invalid peaks conservatively use 0 dBFS.
    pub fn from_sample_peaks(
        outgoing_peak_dbfs: Option<f32>,
        incoming_peak_dbfs: Option<f32>,
        outgoing_base_gain: f32,
        incoming_base_gain: f32,
    ) -> Self {
        let outgoing_peak =
            measured_peak_linear(outgoing_peak_dbfs) * finite_nonnegative_gain(outgoing_base_gain);
        let incoming_peak =
            measured_peak_linear(incoming_peak_dbfs) * finite_nonnegative_gain(incoming_base_gain);
        Self {
            outgoing_peak: finite_peak_or_conservative(outgoing_peak),
            incoming_peak: finite_peak_or_conservative(incoming_peak),
        }
    }

    /// Builds a guard for a planned transition. The outgoing deck is assumed
    /// to retain unity unless a caller supplies a more specific base gain via
    /// [`Self::from_sample_peaks`].
    pub fn from_analyses(
        outgoing: &TrackAnalysis,
        incoming: &TrackAnalysis,
        incoming_base_gain: f32,
    ) -> Self {
        Self::from_analyses_with_base_gains(outgoing, incoming, 1.0, incoming_base_gain)
    }

    /// Builds a guard from true peak (falling back to sample peak) after each
    /// deck's already-applied base gain, such as loudness normalization.
    pub fn from_analyses_with_base_gains(
        outgoing: &TrackAnalysis,
        incoming: &TrackAnalysis,
        outgoing_base_gain: f32,
        incoming_base_gain: f32,
    ) -> Self {
        Self::from_sample_peaks(
            analysis_peak_dbfs(outgoing),
            analysis_peak_dbfs(incoming),
            outgoing_base_gain,
            incoming_base_gain,
        )
    }
}

fn analysis_peak_dbfs(analysis: &TrackAnalysis) -> Option<f32> {
    analysis
        .true_peak_dbtp
        .filter(|peak| peak.is_finite())
        .or(analysis.sample_peak_dbfs)
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
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EqTransition {
    /// Stable identifier used to replace or cancel scheduled automation.
    pub id: u64,
    pub source_start: Duration,
    pub duration: Duration,
    pub role: EqTransitionRole,
    pub harmonic_compatibility: Option<f32>,
}

impl EqTransition {
    /// Returns the equalizer gains at an absolute position on this track's source timeline.
    ///
    /// The incoming deck starts with its low band removed and restores it by the
    /// midpoint. The outgoing deck performs the complementary bass handoff.
    /// The mid/high bands cross over through the whole overlap so vocal and
    /// presence-heavy content does not stay fully open on both decks at once.
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
        let presence_handoff = smoothstep(progress);
        let harmonic_duck = harmonic_presence_duck(self.harmonic_compatibility);
        let (low, mid, high) = match self.role {
            EqTransitionRole::Outgoing => (
                1.0 - bass_handoff,
                1.0 - (0.25 + harmonic_duck) * presence_handoff,
                1.0 - (0.18 + harmonic_duck * 0.7) * presence_handoff,
            ),
            EqTransitionRole::Incoming => (
                bass_handoff,
                (0.7 - harmonic_duck) + (0.3 + harmonic_duck) * presence_handoff,
                (0.82 - harmonic_duck * 0.7) + (0.18 + harmonic_duck * 0.7) * presence_handoff,
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

fn harmonic_presence_duck(score: Option<f32>) -> f32 {
    score
        .filter(|score| score.is_finite())
        .map(|score| ((0.5 - score) / 0.5).clamp(0.0, 1.0) * 0.2)
        .unwrap_or(0.0)
}

#[derive(Clone, Debug, PartialEq)]
pub struct TransitionPlan {
    pub kind: TransitionKind,
    pub outgoing_start: Duration,
    pub incoming_start: Duration,
    pub incoming_cue_selection: Option<AutoMixIncomingCueSelection>,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AutoMixIncomingCueSelection {
    pub default_start: Duration,
    pub selected_start: Duration,
    pub candidates_checked: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AutoMixQualityReport {
    pub issues: Vec<AutoMixQualityIssue>,
    pub overlap: Duration,
    pub beat_pairs_checked: usize,
    /// Fraction of the expected beat-pair slots in the overlap backed by
    /// observed, trusted markers.  `None` means no valid BPM-derived grid.
    pub beat_phase_coverage: Option<f32>,
    pub max_beat_phase_error: Option<Duration>,
    pub handoff_beat_phase_error: Option<Duration>,
    pub downbeat_pairs_checked: usize,
    pub max_downbeat_phase_error: Option<Duration>,
    pub handoff_downbeat_phase_error: Option<Duration>,
    pub phrase_pairs_checked: usize,
    pub max_phrase_phase_error: Option<Duration>,
    pub handoff_phrase_phase_error: Option<Duration>,
    pub phrase_boundary_bars: Option<u8>,
    pub structure_overlap_ratio: Option<f32>,
    pub harmonic_compatibility: Option<f32>,
    pub low_handoff_min: Option<f32>,
    pub low_handoff_max: Option<f32>,
    pub vocal_overlap_samples_checked: usize,
    pub max_dual_vocal_risk: Option<f32>,
    pub energy_samples_checked: usize,
    pub min_mix_energy_ratio: Option<f32>,
    pub max_mix_energy_ratio: Option<f32>,
    pub max_mix_energy_step: Option<f32>,
    pub handoff_mix_energy_ratio: Option<f32>,
    pub handoff_incoming_mix_share: Option<f32>,
    pub max_tempo_speed_step: Option<f32>,
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
pub struct AutoMixScoreBreakdown {
    pub total: f32,
    pub energy_balance_penalty: f32,
    pub vocal_penalty: f32,
    pub short_mix_penalty: f32,
    pub energy_step_penalty: f32,
    pub handoff_energy_penalty: f32,
    pub handoff_ownership_penalty: f32,
    pub tempo_smoothness_penalty: f32,
    pub phrase_strength_penalty: f32,
    pub structure_usage_penalty: f32,
    pub harmonic_overlap_penalty: f32,
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
    DownbeatPhaseUnverified,
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
    MixEnergyDipTooDeep {
        min_ratio: f32,
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
                | Self::DownbeatPhaseUnverified
                | Self::BeatPhaseDriftTooLarge { .. }
                | Self::BeatHandoffPhaseDriftTooLarge { .. }
                | Self::DownbeatPhaseDriftTooLarge { .. }
                | Self::DownbeatHandoffPhaseDriftTooLarge { .. }
                | Self::PhrasePhaseDriftTooLarge { .. }
                | Self::PhraseHandoffPhaseDriftTooLarge { .. }
                | Self::DualVocalOverlapTooHigh { .. }
                | Self::MixEnergyDipTooDeep { .. }
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

    let harmonic_compatibility = harmonic_compatibility(outgoing, incoming);
    if has_trusted_marker_evidence(outgoing)
        && has_trusted_marker_evidence(incoming)
        && let Some(tempo_curve) = compatible_tempo_curve(outgoing, incoming, config)
    {
        let mut best_plan = None;
        let mut best_score = f32::INFINITY;
        let incoming_candidates = safe_incoming_beat_starts(incoming);
        let default_incoming_start = incoming_candidates.first().copied();
        let candidates_checked = incoming_candidates.len();
        for incoming_start in incoming_candidates {
            let mut plan = plan_transition_with_incoming_start(
                outgoing,
                incoming,
                config,
                harmonic_compatibility,
                incoming_start,
                Some(tempo_curve),
                true,
            );
            if plan.kind != TransitionKind::BeatMatched {
                continue;
            }
            plan.incoming_cue_selection =
                default_incoming_start.map(|default_start| AutoMixIncomingCueSelection {
                    default_start,
                    selected_start: incoming_start,
                    candidates_checked,
                });
            let Some(score) = transition_plan_score(outgoing, incoming, &plan) else {
                continue;
            };
            if best_plan.is_none() || score + ENERGY_SELECTION_EPSILON < best_score {
                best_score = score;
                best_plan = Some(plan);
            }
        }
        if let Some(plan) = best_plan {
            return plan;
        }
    }

    plan_transition_with_incoming_start(
        outgoing,
        incoming,
        config,
        harmonic_compatibility,
        incoming.audible_start,
        None,
        false,
    )
}

#[allow(clippy::too_many_arguments)]
fn plan_transition_with_incoming_start(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    config: &AutoMixConfig,
    harmonic_compatibility: Option<f32>,
    selected_incoming_start: Duration,
    tempo_curve: Option<(f32, f32)>,
    beat_aligned: bool,
) -> TransitionPlan {
    let beat_aligned = beat_aligned && tempo_curve.is_some();
    let mut use_beatmatch = beat_aligned;
    // Track loudness is normalized independently by the playback layer. The
    // planner must not apply a second pairwise RMS correction.
    let incoming_gain = 1.0;
    let incoming_start = if beat_aligned {
        selected_incoming_start
    } else {
        incoming.audible_start
    };
    let available_outgoing = outgoing.audible_end.saturating_sub(outgoing.audible_start);
    let available_incoming = incoming.audible_end.saturating_sub(incoming_start);
    let Some(timing) =
        plan_transition_timing(available_outgoing, available_incoming, config.crossfade)
    else {
        return gapless_plan(outgoing, incoming);
    };
    let base_duration = if harmonic_compatibility.is_some_and(|score| score < 0.5) {
        timing.fade_duration.min(Duration::from_secs(4))
    } else {
        timing.fade_duration
    };
    let structured_duration =
        trusted_structure_overlap(outgoing, incoming, incoming_start, base_duration);
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
                && incoming.downbeat_confidence >= MIN_ACTIONABLE_DOWNBEAT_CONFIDENCE
                && incoming.first_downbeat.is_some())
            .then(|| {
                align_to_strongest_matching_phrase_phase(
                    outgoing,
                    incoming,
                    incoming_start,
                    target,
                    target_duration,
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
            align_to_strongest_matching_phrase_phase(
                outgoing,
                incoming,
                incoming_start,
                target,
                exact_vocal_limit,
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
        incoming_cue_selection: None,
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
    plan_guarded_transition_with_base_gains(outgoing, incoming, config, 1.0, 1.0)
}

/// Plans and guards a transition using the same per-deck gains that playback
/// will apply before its mix curve.
pub fn plan_guarded_transition_with_base_gains(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    config: &AutoMixConfig,
    outgoing_base_gain: f32,
    incoming_base_gain: f32,
) -> GuardedTransitionPlan {
    let plan = plan_transition(outgoing, incoming, config);
    let quality = evaluate_transition_quality_with_base_gains(
        outgoing,
        incoming,
        &plan,
        outgoing_base_gain,
        incoming_base_gain,
    );
    if !quality.has_blocking_issue() {
        return GuardedTransitionPlan {
            plan,
            quality,
            rejected_plan: None,
            rejected_quality: None,
        };
    }

    let conservative = conservative_guarded_transition_with_base_gains(
        outgoing,
        incoming,
        config,
        outgoing_base_gain,
        incoming_base_gain,
    );
    GuardedTransitionPlan {
        plan: conservative.plan,
        quality: conservative.quality,
        rejected_plan: Some(plan),
        rejected_quality: Some(quality),
    }
}

/// Plans a guarded transition while explicitly excluding beat matching.
///
/// Runtime callers that need a deterministic non-beatmatched fallback can use
/// this helper without mutating the analyses or weakening the beat evidence.
/// It applies the same conservative-crossfade and quality gates as the normal
/// guarded planner, then uses a gapless handoff when that fallback is unsafe.
pub fn plan_guarded_non_beatmatched_transition_with_base_gains(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    config: &AutoMixConfig,
    outgoing_base_gain: f32,
    incoming_base_gain: f32,
) -> GuardedTransitionPlan {
    conservative_guarded_transition_with_base_gains(
        outgoing,
        incoming,
        config,
        outgoing_base_gain,
        incoming_base_gain,
    )
}

fn conservative_guarded_transition_with_base_gains(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    config: &AutoMixConfig,
    outgoing_base_gain: f32,
    incoming_base_gain: f32,
) -> GuardedTransitionPlan {
    let fallback = conservative_crossfade_plan(outgoing, incoming, config);
    let fallback_quality = evaluate_transition_quality_with_base_gains(
        outgoing,
        incoming,
        &fallback,
        outgoing_base_gain,
        incoming_base_gain,
    );
    if !fallback_quality.has_blocking_issue() {
        return GuardedTransitionPlan {
            plan: fallback,
            quality: fallback_quality,
            rejected_plan: None,
            rejected_quality: None,
        };
    }

    let gapless = gapless_plan(outgoing, incoming);
    let gapless_quality = evaluate_transition_quality_with_base_gains(
        outgoing,
        incoming,
        &gapless,
        outgoing_base_gain,
        incoming_base_gain,
    );
    GuardedTransitionPlan {
        plan: gapless,
        quality: gapless_quality,
        rejected_plan: Some(fallback),
        rejected_quality: Some(fallback_quality),
    }
}

fn transition_plan_score(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    plan: &TransitionPlan,
) -> Option<f32> {
    let quality = evaluate_transition_quality(outgoing, incoming, plan);
    let blocking_penalty = if quality.has_blocking_issue() {
        1_000.0 + quality.issues.len() as f32
    } else {
        0.0
    };
    transition_start_score(&quality)
        .or_else(|| quality.is_ok().then_some(0.0))
        .map(|score| {
            let marker_penalty = if plan.kind == TransitionKind::BeatMatched {
                marker_evidence_penalty(outgoing, incoming, &quality)
            } else {
                0.0
            };
            score + blocking_penalty + marker_penalty
        })
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
    if !tempo_alignment_confident(outgoing, config.min_beat_confidence) {
        return AutoMixBeatMatchDecision::OutgoingTempoConfidenceTooLow;
    }
    if !tempo_alignment_confident(incoming, config.min_beat_confidence) {
        return AutoMixBeatMatchDecision::IncomingTempoConfidenceTooLow;
    }
    if safe_incoming_beat_start(incoming).is_none() {
        return AutoMixBeatMatchDecision::NoTrustedIncomingBeatStart;
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
    evaluate_transition_quality_with_base_gains(outgoing, incoming, plan, 1.0, 1.0)
}

/// Evaluates the rendered transition model after each deck's persistent base
/// gain (for example loudness normalization) and before the shared master
/// volume.
pub fn evaluate_transition_quality_with_base_gains(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    plan: &TransitionPlan,
    outgoing_base_gain: f32,
    incoming_base_gain: f32,
) -> AutoMixQualityReport {
    let mut issues = Vec::new();
    let mut beat_pairs_checked = 0;
    let mut beat_phase_coverage = None;
    let mut max_beat_phase_error = None;
    let mut handoff_beat_phase_error = None;
    let mut downbeat_pairs_checked = 0;
    let mut max_downbeat_phase_error = None;
    let mut handoff_downbeat_phase_error = None;
    let mut phrase_pairs_checked = 0;
    let mut max_phrase_phase_error = None;
    let mut handoff_phrase_phase_error = None;
    let mut phrase_boundary_bars = None;
    let structure_overlap_ratio = structure_overlap_usage_ratio(outgoing, incoming, plan);
    let mut vocal_overlap_samples_checked = 0;
    let mut max_dual_vocal_risk = None;
    let mut energy_samples_checked = 0;
    let mut min_mix_energy_ratio = None;
    let mut max_mix_energy_ratio = None;
    let mut max_mix_energy_step = None;
    let mut handoff_mix_energy_ratio = None;
    let mut handoff_incoming_mix_share = None;
    let max_tempo_speed_step = tempo_speed_step(plan.tempo_envelope);
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

        let vocal_overlap = dual_vocal_overlap_report(
            outgoing,
            incoming,
            plan,
            outgoing_base_gain,
            incoming_base_gain,
        );
        vocal_overlap_samples_checked = vocal_overlap.samples;
        max_dual_vocal_risk = vocal_overlap.max_risk;
        if let Some(max_risk) = max_dual_vocal_risk
            && max_risk > MAX_DUAL_VOCAL_RISK
        {
            issues.push(AutoMixQualityIssue::DualVocalOverlapTooHigh { max_risk });
        }

        let energy = transition_energy_report(
            outgoing,
            incoming,
            plan,
            outgoing_base_gain,
            incoming_base_gain,
        );
        energy_samples_checked = energy.samples;
        min_mix_energy_ratio = energy.min_ratio;
        max_mix_energy_ratio = energy.max_ratio;
        max_mix_energy_step = energy.max_step;
        handoff_mix_energy_ratio = energy.handoff_ratio;
        handoff_incoming_mix_share = energy.handoff_incoming_share;
        if let Some(min_ratio) = min_mix_energy_ratio
            && mix_energy_dip_is_blocking(min_ratio)
            && energy.longest_blocking_gap >= MIN_BLOCKING_ENERGY_GAP
        {
            issues.push(AutoMixQualityIssue::MixEnergyDipTooDeep { min_ratio });
        }
    }

    if plan.kind == TransitionKind::BeatMatched {
        let phase = beat_phase_report(outgoing, incoming, plan);
        beat_pairs_checked = phase.pairs;
        beat_phase_coverage = phase.coverage;
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
        } else if actionable_downbeat_confidence(outgoing)
            && actionable_downbeat_confidence(incoming)
            && downbeat_phase.max_error.is_none()
        {
            // Actionable phase metadata without a corresponding observed
            // marker pair is not evidence of a safe downbeat handoff.
            issues.push(AutoMixQualityIssue::DownbeatPhaseUnverified);
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
        phrase_boundary_bars = strongest_phrase_boundary_bars(outgoing, incoming, plan);
    }

    AutoMixQualityReport {
        issues,
        overlap: plan.duration,
        beat_pairs_checked,
        beat_phase_coverage,
        max_beat_phase_error,
        handoff_beat_phase_error,
        downbeat_pairs_checked,
        max_downbeat_phase_error,
        handoff_downbeat_phase_error,
        phrase_pairs_checked,
        max_phrase_phase_error,
        handoff_phrase_phase_error,
        phrase_boundary_bars,
        structure_overlap_ratio,
        harmonic_compatibility: plan.harmonic_compatibility,
        low_handoff_min,
        low_handoff_max,
        vocal_overlap_samples_checked,
        max_dual_vocal_risk,
        energy_samples_checked,
        min_mix_energy_ratio,
        max_mix_energy_ratio,
        max_mix_energy_step,
        handoff_mix_energy_ratio,
        handoff_incoming_mix_share,
        max_tempo_speed_step,
    }
}

fn tempo_speed_step(envelope: Option<TempoEnvelope>) -> Option<f32> {
    let envelope = envelope?;
    let mut previous = envelope.initial_speed;
    if !previous.is_finite() {
        return None;
    }
    let mut max_step = 0.0_f32;
    for segment in &envelope.phase_segments[..usize::from(envelope.phase_segment_count)] {
        if !segment.speed.is_finite() || !previous.is_finite() {
            return None;
        }
        max_step = max_step.max((segment.speed - previous).abs());
        previous = segment.speed;
    }
    if !envelope.mix_end_speed.is_finite() || !max_step.is_finite() {
        return None;
    }
    max_step = max_step.max((envelope.mix_end_speed - previous).abs());
    Some(max_step)
}

#[derive(Clone, Copy)]
struct BeatPhaseReport {
    pairs: usize,
    coverage: Option<f32>,
    max_error: Option<Duration>,
}

#[derive(Clone, Copy)]
struct BeatAlignmentEvidence {
    phase: BeatPhaseReport,
    handoff_error: Option<Duration>,
}

#[derive(Clone, Copy)]
struct ObservedMarkerPair {
    outgoing_index: usize,
    incoming_index: usize,
    outgoing: Duration,
    incoming: Duration,
    error: Duration,
}

fn beat_phase_report(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    plan: &TransitionPlan,
) -> BeatPhaseReport {
    beat_alignment_evidence(outgoing, incoming, plan).phase
}

fn beat_alignment_evidence(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    plan: &TransitionPlan,
) -> BeatAlignmentEvidence {
    let marker_pairs = observed_marker_pairs(outgoing, incoming, plan);
    let pairs = marker_pairs.len();
    let coverage = beat_phase_coverage(outgoing, incoming, plan, pairs);
    let max_error = (pairs >= MIN_BEAT_PHASE_PAIRS
        && coverage.is_some_and(|coverage| coverage >= MIN_BEAT_PHASE_COVERAGE))
    .then(|| marker_pairs.iter().map(|pair| pair.error).max())
    .flatten();
    let outgoing_end = plan.outgoing_start.saturating_add(plan.duration);
    let incoming_end = plan
        .incoming_start
        .saturating_add(incoming_mix_source(plan));
    let handoff_error = marker_pairs.last().and_then(|pair| {
        let outgoing_tolerance = beat_interval(outgoing)?.div_f64(2.0);
        let incoming_tolerance = beat_interval(incoming)?.div_f64(2.0);
        (pair.outgoing.abs_diff(outgoing_end) <= outgoing_tolerance
            && pair.incoming.abs_diff(incoming_end) <= incoming_tolerance)
            .then_some(pair.error)
    });
    BeatAlignmentEvidence {
        phase: BeatPhaseReport {
            pairs,
            coverage,
            max_error,
        },
        handoff_error,
    }
}

fn beat_phase_coverage(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    plan: &TransitionPlan,
    observed_pairs: usize,
) -> Option<f32> {
    let outgoing_interval = beat_interval(outgoing)?;
    let (outgoing_stride, _) = beat_family_strides(outgoing, incoming, plan);
    let expected_pair_interval = outgoing_interval.mul_f64(outgoing_stride as f64);
    let expected_pairs = (plan.duration.as_secs_f64() / expected_pair_interval.as_secs_f64())
        .ceil()
        .max(1.0);
    let coverage = observed_pairs as f64 / expected_pairs;
    coverage.is_finite().then_some((coverage as f32).min(1.0))
}

fn observed_marker_pairs(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    plan: &TransitionPlan,
) -> Vec<ObservedMarkerPair> {
    if plan.duration.is_zero() {
        return Vec::new();
    }
    let outgoing_end = plan.outgoing_start.saturating_add(plan.duration);
    let incoming_end = plan
        .incoming_start
        .saturating_add(incoming_mix_source(plan));
    let Some(outgoing_anchor) = marker_anchor_index(outgoing, plan.outgoing_start) else {
        return Vec::new();
    };
    let Some(incoming_anchor) = marker_anchor_index(incoming, plan.incoming_start) else {
        return Vec::new();
    };
    let (outgoing_stride, incoming_stride) = beat_family_strides(outgoing, incoming, plan);
    let mut marker_pairs = Vec::new();
    let mut step = 0_usize;
    while marker_pairs.len() < MAX_PHASE_MARKER_PAIRS {
        let Some(outgoing_index) =
            outgoing_anchor.checked_add(step.saturating_mul(outgoing_stride))
        else {
            break;
        };
        let Some(incoming_index) =
            incoming_anchor.checked_add(step.saturating_mul(incoming_stride))
        else {
            break;
        };
        let (Some(&outgoing_beat), Some(&incoming_beat)) = (
            outgoing.beat_markers.get(outgoing_index),
            incoming.beat_markers.get(incoming_index),
        ) else {
            break;
        };
        if outgoing_beat > outgoing_end || incoming_beat > incoming_end {
            break;
        }
        let output_elapsed = outgoing_beat.saturating_sub(plan.outgoing_start);
        let incoming_position = plan
            .incoming_start
            .saturating_add(incoming_source_elapsed(plan, output_elapsed));
        let error = incoming_beat.abs_diff(incoming_position);
        if !marker_is_trusted(outgoing, outgoing_index)
            || !marker_is_trusted(incoming, incoming_index)
        {
            step += 1;
            continue;
        }
        marker_pairs.push(ObservedMarkerPair {
            outgoing_index,
            incoming_index,
            outgoing: outgoing_beat,
            incoming: incoming_beat,
            error,
        });
        step += 1;
    }
    marker_pairs
}

fn marker_anchor_index(analysis: &TrackAnalysis, position: Duration) -> Option<usize> {
    analysis
        .beat_markers
        .iter()
        .copied()
        .enumerate()
        .filter(|(index, _)| marker_is_trusted(analysis, *index))
        .min_by_key(|(_, marker)| marker.abs_diff(position))
        .filter(|(_, marker)| marker.abs_diff(position) <= marker_snap_tolerance(analysis))
        .map(|(index, _)| index)
}

fn trusted_marker_position(analysis: &TrackAnalysis, position: Duration) -> bool {
    analysis
        .beat_markers
        .iter()
        .copied()
        .enumerate()
        .any(|(index, marker)| marker == position && marker_is_trusted(analysis, index))
}

fn beat_family_strides(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    plan: &TransitionPlan,
) -> (usize, usize) {
    beat_family_strides_for_speed(outgoing, incoming, plan.incoming_tempo_ratio)
}

fn beat_family_strides_for_speed(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    incoming_speed: f32,
) -> (usize, usize) {
    let Some(outgoing_bpm) = outgoing.bpm.filter(|bpm| bpm.is_finite() && *bpm > 0.0) else {
        return (1, 1);
    };
    let Some(incoming_bpm) = incoming.bpm.filter(|bpm| bpm.is_finite() && *bpm > 0.0) else {
        return (1, 1);
    };
    let effective_incoming = incoming_bpm * incoming_speed;
    let ratio = effective_incoming / outgoing_bpm;
    if (1.5..=2.5).contains(&ratio) {
        (1, 2)
    } else if (0.4..=0.75).contains(&ratio) {
        (2, 1)
    } else {
        (1, 1)
    }
}

fn downbeat_phase_report(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    plan: &TransitionPlan,
) -> BeatPhaseReport {
    observed_phase_report(outgoing, incoming, plan, 4, MIN_DOWNBEAT_PHASE_PAIRS)
}

fn phrase_phase_report(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    plan: &TransitionPlan,
    length: PhraseLength,
) -> BeatPhaseReport {
    observed_phase_report(
        outgoing,
        incoming,
        plan,
        4_u32.saturating_mul(length.bars()),
        MIN_PHRASE_PHASE_PAIRS,
    )
}

fn observed_phase_report(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    plan: &TransitionPlan,
    cycle_beats: u32,
    minimum_pairs: usize,
) -> BeatPhaseReport {
    if cycle_beats == 0
        || !actionable_downbeat_confidence(outgoing)
        || !actionable_downbeat_confidence(incoming)
    {
        return BeatPhaseReport {
            pairs: 0,
            coverage: None,
            max_error: None,
        };
    }
    let marker_pairs = observed_marker_pairs(outgoing, incoming, plan);
    let pairs = marker_pairs
        .iter()
        .filter(|pair| {
            marker_is_near_cycle_boundary(outgoing, pair.outgoing_index, cycle_beats)
                && marker_is_near_cycle_boundary(incoming, pair.incoming_index, cycle_beats)
        })
        .collect::<Vec<_>>();
    let max_error = (pairs.len() >= minimum_pairs)
        .then(|| pairs.iter().map(|pair| pair.error).max())
        .flatten();
    BeatPhaseReport {
        pairs: pairs.len(),
        coverage: None,
        max_error,
    }
}

fn actionable_downbeat_confidence(analysis: &TrackAnalysis) -> bool {
    analysis.downbeat_confidence.is_finite()
        && analysis.downbeat_confidence >= MIN_ACTIONABLE_DOWNBEAT_CONFIDENCE
}

fn marker_is_near_cycle_boundary(
    analysis: &TrackAnalysis,
    marker_index: usize,
    cycle_beats: u32,
) -> bool {
    if cycle_beats == 0 || !marker_is_trusted(analysis, marker_index) {
        return false;
    }
    // Phase is a property of the observed marker ordinal, not of an
    // extrapolated first_downbeat+BPM clock.  The latter accumulates local
    // tempo drift over long tracks and can mark every real phrase marker as
    // unverified even when the two decks have a stable observed alignment.
    let Some(first_downbeat) = analysis.first_downbeat else {
        return false;
    };
    let Some(origin_index) = analysis
        .beat_markers
        .iter()
        .copied()
        .enumerate()
        .filter(|(index, marker)| {
            marker_is_trusted(analysis, *index)
                && marker.abs_diff(first_downbeat) <= marker_snap_tolerance(analysis)
        })
        .min_by_key(|(_, marker)| marker.abs_diff(first_downbeat))
        .map(|(index, _)| index)
    else {
        return false;
    };
    marker_index.abs_diff(origin_index) % cycle_beats as usize == 0
}

fn beat_handoff_phase_error(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    plan: &TransitionPlan,
) -> Option<Duration> {
    beat_alignment_evidence(outgoing, incoming, plan).handoff_error
}

fn downbeat_handoff_phase_error(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    plan: &TransitionPlan,
) -> Option<Duration> {
    observed_phase_handoff_error(outgoing, incoming, plan, 4, MIN_DOWNBEAT_PHASE_PAIRS)
}

fn phrase_handoff_phase_error(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    plan: &TransitionPlan,
    length: PhraseLength,
) -> Option<Duration> {
    observed_phase_handoff_error(
        outgoing,
        incoming,
        plan,
        4_u32.saturating_mul(length.bars()),
        MIN_PHRASE_PHASE_PAIRS,
    )
}

fn observed_phase_handoff_error(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    plan: &TransitionPlan,
    cycle_beats: u32,
    minimum_pairs: usize,
) -> Option<Duration> {
    if !actionable_downbeat_confidence(outgoing) || !actionable_downbeat_confidence(incoming) {
        return None;
    }
    let outgoing_interval = beat_interval(outgoing)?.mul_f64(f64::from(cycle_beats));
    let incoming_interval = beat_interval(incoming)?.mul_f64(f64::from(cycle_beats));
    let outgoing_end = plan.outgoing_start.saturating_add(plan.duration);
    let incoming_end = plan
        .incoming_start
        .saturating_add(incoming_mix_source(plan));
    let phase_pairs = observed_marker_pairs(outgoing, incoming, plan)
        .into_iter()
        .filter(|pair| {
            marker_is_near_cycle_boundary(outgoing, pair.outgoing_index, cycle_beats)
                && marker_is_near_cycle_boundary(incoming, pair.incoming_index, cycle_beats)
        })
        .collect::<Vec<_>>();
    (phase_pairs.len() >= minimum_pairs)
        .then(|| {
            phase_pairs
                .into_iter()
                .rev()
                .find(|pair| {
                    pair.outgoing.abs_diff(outgoing_end)
                        <= beat_interval(outgoing)
                            .unwrap_or(outgoing_interval)
                            .div_f64(2.0)
                        && pair.incoming.abs_diff(incoming_end)
                            <= beat_interval(incoming)
                                .unwrap_or(incoming_interval)
                                .div_f64(2.0)
                })
                .map(|pair| pair.error)
        })
        .flatten()
}

fn strongest_phrase_boundary_bars(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    plan: &TransitionPlan,
) -> Option<u8> {
    if plan.kind != TransitionKind::BeatMatched {
        return None;
    }
    [
        PhraseLength::SixteenBars,
        PhraseLength::EightBars,
        PhraseLength::FourBars,
    ]
    .into_iter()
    .find_map(|length| {
        (phrase_phase_is_actionable(outgoing, incoming, plan, length)
            && phrase_start_phase_error(outgoing, incoming, plan, length)
                .is_some_and(|error| error <= MAX_PHRASE_PHASE_ERROR))
        .then_some(length.bars() as u8)
    })
}

fn phrase_start_phase_error(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    plan: &TransitionPlan,
    length: PhraseLength,
) -> Option<Duration> {
    observed_phase_report(
        outgoing,
        incoming,
        plan,
        4_u32.saturating_mul(length.bars()),
        MIN_PHRASE_PHASE_PAIRS,
    )
    .max_error
}

fn phrase_phase_is_actionable(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    plan: &TransitionPlan,
    length: PhraseLength,
) -> bool {
    if plan.harmonic_compatibility.is_some_and(|score| score < 0.5) {
        return false;
    }
    let Some(outgoing_grid) = outgoing.beat_grid() else {
        return false;
    };
    let Some(incoming_grid) = incoming.beat_grid() else {
        return false;
    };
    if !actionable_downbeat_confidence(outgoing) || !actionable_downbeat_confidence(incoming) {
        return false;
    }
    let outgoing_phrase = phrase_interval(outgoing_grid, length);
    let incoming_phrase = phrase_interval(incoming_grid, length);
    if outgoing_phrase.is_zero() || incoming_phrase.is_zero() {
        return false;
    }
    let minimum_overlap = outgoing_phrase.min(incoming_phrase).div_f64(2.0);
    plan.duration >= minimum_overlap
}

fn trusted_beat_markers_between_limited(
    analysis: &TrackAnalysis,
    start: Duration,
    end: Duration,
    limit: usize,
) -> Vec<Duration> {
    if limit == 0 {
        return Vec::new();
    }

    let mut markers = Vec::new();
    // This helper is only used by incoming-cue enumeration. Bound the input
    // scan as well as the output so malformed marker arrays cannot consume
    // unbounded CPU when none of their entries are trusted.
    for (index, beat) in analysis
        .beat_markers
        .iter()
        .copied()
        .take(MAX_INCOMING_CUE_CANDIDATES)
        .enumerate()
    {
        if beat >= start && beat <= end && marker_is_trusted(analysis, index) {
            markers.push(beat);
            if markers.len() >= limit {
                break;
            }
        }
    }
    markers
}

fn downbeat_positions_between_limited(
    analysis: &TrackAnalysis,
    start: Duration,
    end: Duration,
    limit: usize,
) -> Vec<Duration> {
    if end <= start {
        return Vec::new();
    }
    let Some(grid) = analysis.beat_grid() else {
        return Vec::new();
    };
    if grid.downbeat_confidence < MIN_ACTIONABLE_DOWNBEAT_CONFIDENCE {
        return Vec::new();
    }

    let interval = grid.beat_interval.mul_f64(f64::from(grid.beats_per_bar));
    if interval.is_zero() {
        return Vec::new();
    }

    cycle_positions_between_limited(grid.first_downbeat, interval, start, end, limit)
}

fn cycle_positions_between_limited(
    first: Duration,
    interval: Duration,
    start: Duration,
    end: Duration,
    limit: usize,
) -> Vec<Duration> {
    if limit == 0 {
        return Vec::new();
    }

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
    while position <= end.as_secs_f64() && positions.len() < limit {
        positions.push(Duration::from_secs_f64(position));
        let next = position + interval_secs;
        if !next.is_finite() || next <= position {
            break;
        }
        position = next;
    }
    positions
}

fn beat_interval(analysis: &TrackAnalysis) -> Option<Duration> {
    let bpm = analysis.bpm?;
    if bpm <= 0.0 || !bpm.is_finite() {
        return None;
    }
    beat_interval_from_bpm(bpm)
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
        harmonic_compatibility: plan.harmonic_compatibility,
    };
    let incoming = EqTransition {
        id: 0,
        source_start: plan.incoming_start,
        duration: incoming_mix_source(plan),
        role: EqTransitionRole::Incoming,
        harmonic_compatibility: plan.harmonic_compatibility,
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
    outgoing_base_gain: f32,
    incoming_base_gain: f32,
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

    let outgoing_base_gain = finite_nonnegative_gain(outgoing_base_gain);
    let incoming_base_gain = finite_nonnegative_gain(
        finite_nonnegative_gain(incoming_base_gain) * finite_nonnegative_gain(plan.incoming_gain),
    );
    let mut samples = 0;
    let mut max_risk = None;
    let peak_guard = AutoMixPeakGuard::from_analyses_with_base_gains(
        outgoing,
        incoming,
        outgoing_base_gain,
        incoming_base_gain,
    );
    // Vocal collision is a relative masking risk, so a common loudness target
    // must not make it disappear. Only the balance between decks belongs in
    // this metric; the absolute gains remain relevant to the peak guard above.
    let vocal_gain_reference = outgoing_base_gain.max(incoming_base_gain).max(1.0e-6);
    let outgoing_vocal_gain = outgoing_base_gain / vocal_gain_reference;
    let incoming_vocal_gain = incoming_base_gain / vocal_gain_reference;
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
        let (outgoing_mix_gain, incoming_mix_gain) =
            automix_peak_safe_mix_gains(plan.kind, progress, peak_guard);
        let outgoing_eq = EqTransition {
            id: 0,
            source_start: plan.outgoing_start,
            duration: plan.duration,
            role: EqTransitionRole::Outgoing,
            harmonic_compatibility: plan.harmonic_compatibility,
        }
        .gains_at(outgoing_position);
        let incoming_eq = EqTransition {
            id: 0,
            source_start: plan.incoming_start,
            duration: incoming_mix_source(plan),
            role: EqTransitionRole::Incoming,
            harmonic_compatibility: plan.harmonic_compatibility,
        }
        .gains_at(incoming_position);
        let outgoing_risk = effective_vocal_risk(outgoing, outgoing_position)
            * outgoing_mix_gain
            * outgoing_vocal_gain
            * vocal_band_gain(outgoing_eq);
        let incoming_risk = effective_vocal_risk(incoming, incoming_position)
            * incoming_mix_gain
            * incoming_vocal_gain
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
    max_step: Option<f32>,
    longest_blocking_gap: Duration,
    handoff_ratio: Option<f32>,
    handoff_incoming_share: Option<f32>,
}

fn transition_energy_report(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    plan: &TransitionPlan,
    outgoing_base_gain: f32,
    incoming_base_gain: f32,
) -> TransitionEnergyReport {
    const SAMPLES: u32 = 32;
    if plan.kind == TransitionKind::Gapless || plan.duration.is_zero() {
        return TransitionEnergyReport {
            samples: 0,
            min_ratio: None,
            max_ratio: None,
            max_step: None,
            longest_blocking_gap: Duration::ZERO,
            handoff_ratio: None,
            handoff_incoming_share: None,
        };
    }
    let outgoing_base_gain = finite_nonnegative_gain(outgoing_base_gain);
    let incoming_base_gain = finite_nonnegative_gain(
        finite_nonnegative_gain(incoming_base_gain) * finite_nonnegative_gain(plan.incoming_gain),
    );
    // Edge windows can both land inside a quiet bar and make the whole mix
    // look healthy after normalization. Derive each deck's reference from a
    // bounded source-available region around the planned overlap instead.
    // The 75th percentile ignores isolated spikes while still retaining a
    // nearby normal-energy floor when a meaningful portion of the source is
    // available around a sustained, avoidable gap.
    let outgoing_reference = robust_energy_reference(
        outgoing,
        plan.outgoing_start,
        plan.outgoing_start.saturating_add(plan.duration),
    )
    .map(|energy| energy * outgoing_base_gain);
    let incoming_reference = robust_energy_reference(
        incoming,
        plan.incoming_start,
        plan.incoming_start
            .saturating_add(incoming_mix_source(plan)),
    )
    .map(|energy| energy * incoming_base_gain);
    let Some((outgoing_reference, incoming_reference)) = outgoing_reference
        .zip(incoming_reference)
        .filter(|(outgoing, incoming)| {
            outgoing.is_finite()
                && incoming.is_finite()
                && *outgoing >= 0.0
                && *incoming >= 0.0
                && (*outgoing).max(*incoming) > 1.0e-6
        })
    else {
        return TransitionEnergyReport {
            samples: 0,
            min_ratio: None,
            max_ratio: None,
            max_step: None,
            longest_blocking_gap: Duration::ZERO,
            handoff_ratio: None,
            handoff_incoming_share: None,
        };
    };
    let reference = outgoing_reference.max(incoming_reference).max(1.0e-6);

    let mut samples = 0;
    let mut actionable_samples = 0;
    let mut min_ratio = f32::INFINITY;
    let mut max_ratio = f32::NEG_INFINITY;
    let mut previous_ratio: Option<f32> = None;
    let mut max_step = 0.0_f32;
    let mut handoff_incoming_share = f32::INFINITY;
    let mut handoff_share_samples = 0;
    let sample_span = plan.duration.div_f64(f64::from(SAMPLES));
    let mut current_blocking_gap = Duration::ZERO;
    let mut current_structural_break_gap = Duration::ZERO;
    let mut longest_blocking_gap = Duration::ZERO;
    let peak_guard = AutoMixPeakGuard::from_analyses_with_base_gains(
        outgoing,
        incoming,
        outgoing_base_gain,
        incoming_base_gain,
    );
    for index in 0..=SAMPLES {
        let elapsed = plan.duration.mul_f64(f64::from(index) / f64::from(SAMPLES));
        let outgoing_position = plan.outgoing_start.saturating_add(elapsed);
        let incoming_position = plan
            .incoming_start
            .saturating_add(incoming_source_elapsed(plan, elapsed));
        let (Some(outgoing_energy), Some(incoming_energy)) = (
            windowed_energy_at(outgoing, outgoing_position, MIN_ENERGY_WINDOW),
            windowed_energy_at(incoming, incoming_position, MIN_ENERGY_WINDOW),
        ) else {
            continue;
        };

        let progress = elapsed.as_secs_f32() / plan.duration.as_secs_f32();
        let (outgoing_mix_gain, incoming_mix_gain) =
            automix_peak_safe_mix_gains(plan.kind, progress, peak_guard);
        let outgoing_eq = EqTransition {
            id: 0,
            source_start: plan.outgoing_start,
            duration: plan.duration,
            role: EqTransitionRole::Outgoing,
            harmonic_compatibility: plan.harmonic_compatibility,
        }
        .gains_at(outgoing_position);
        let incoming_eq = EqTransition {
            id: 0,
            source_start: plan.incoming_start,
            duration: incoming_mix_source(plan),
            role: EqTransitionRole::Incoming,
            harmonic_compatibility: plan.harmonic_compatibility,
        }
        .gains_at(incoming_position);
        let outgoing_level =
            outgoing_energy * outgoing_mix_gain * outgoing_base_gain * full_band_gain(outgoing_eq);
        let incoming_level =
            incoming_energy * incoming_mix_gain * incoming_base_gain * full_band_gain(incoming_eq);
        let combined = (outgoing_level.mul_add(outgoing_level, incoming_level * incoming_level))
            .sqrt()
            / reference;
        let outgoing_source_ratio = outgoing_energy * outgoing_base_gain / outgoing_reference;
        let incoming_source_ratio = incoming_energy * incoming_base_gain / incoming_reference;
        let both_sources_in_structural_break = outgoing_source_ratio.is_finite()
            && incoming_source_ratio.is_finite()
            && outgoing_source_ratio < MIN_STRUCTURAL_BREAK_RATIO
            && incoming_source_ratio < MIN_STRUCTURAL_BREAK_RATIO;
        let total_power = outgoing_level.mul_add(outgoing_level, incoming_level * incoming_level);
        if progress >= 0.75 && total_power > 1.0e-12 {
            let incoming_share = (incoming_level * incoming_level / total_power).clamp(0.0, 1.0);
            handoff_incoming_share = handoff_incoming_share.min(incoming_share);
            handoff_share_samples += 1;
        }
        samples += 1;
        let mut include_in_minimum = !both_sources_in_structural_break;
        if !both_sources_in_structural_break {
            actionable_samples += 1;
        }
        max_ratio = max_ratio.max(combined);
        if mix_energy_dip_is_blocking(combined) {
            current_blocking_gap = current_blocking_gap
                .saturating_add(sample_span)
                .min(plan.duration);
            if both_sources_in_structural_break {
                current_structural_break_gap = current_structural_break_gap
                    .saturating_add(sample_span)
                    .min(plan.duration);
                include_in_minimum = include_in_minimum
                    || current_structural_break_gap > MAX_STRUCTURAL_BREAK_DURATION;
            } else {
                current_structural_break_gap = Duration::ZERO;
            }
            // A simultaneous source break is an unavoidable low-energy
            // passage only after it persists for the same blocking window.
            // Keep shorter breaks eligible for the normal continuous-gap
            // guard, and discard the accumulated run once the break is
            // proven structural.
            if (MIN_BLOCKING_ENERGY_GAP..=MAX_STRUCTURAL_BREAK_DURATION)
                .contains(&current_structural_break_gap)
            {
                current_blocking_gap = Duration::ZERO;
            } else {
                longest_blocking_gap = longest_blocking_gap.max(current_blocking_gap);
            }
        } else {
            current_blocking_gap = Duration::ZERO;
            current_structural_break_gap = Duration::ZERO;
        }
        if include_in_minimum {
            min_ratio = min_ratio.min(combined);
            if both_sources_in_structural_break {
                actionable_samples += 1;
            }
        }
        if let Some(previous) = previous_ratio {
            max_step = max_step.max((combined - previous).abs());
        }
        previous_ratio = Some(combined);
    }

    if current_structural_break_gap < MIN_BLOCKING_ENERGY_GAP
        || current_structural_break_gap > MAX_STRUCTURAL_BREAK_DURATION
    {
        longest_blocking_gap = longest_blocking_gap.max(current_blocking_gap);
    }

    TransitionEnergyReport {
        samples,
        // A simultaneous structural break is score-only: do not let its
        // unavoidable silence become the minimum used by the blocking gate.
        // Keep a finite neutral value when the entire overlap is structural.
        min_ratio: (actionable_samples > 0)
            .then_some(min_ratio)
            .or_else(|| (samples > 0).then_some(1.0)),
        max_ratio: (samples > 0).then_some(max_ratio),
        max_step: (samples > 1).then_some(max_step),
        longest_blocking_gap,
        handoff_ratio: previous_ratio,
        handoff_incoming_share: (handoff_share_samples > 0).then_some(handoff_incoming_share),
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

    // A vocal/structure constraint can leave only a very short initial
    // overlap.  Still inspect the bounded marker history before accepting
    // that short candidate: a nearby trusted marker farther toward the
    // outgoing end may provide the required observed phase evidence.  The
    // vocal and energy gates below remain authoritative, so this cannot turn
    // an unsafe long overlap into an accepted one.
    let marker_search_window = if beat_aligned && search_window < MIN_NATURAL_MIX_OVERLAP {
        MAX_SHORTER_TRANSITION_SEARCH
    } else {
        search_window
    };

    let mut candidates = vec![default_start];
    if beat_aligned {
        if harmonic_compatibility.is_none_or(|score| score >= 0.5)
            && incoming.downbeat_confidence >= MIN_ACTIONABLE_DOWNBEAT_CONFIDENCE
            && incoming.first_downbeat.is_some()
        {
            candidates.extend(matching_phrase_phase_candidates(
                outgoing,
                incoming,
                incoming_start,
                target,
                marker_search_window,
                PhraseLength::FourBars,
                phase_bias,
            ));
        }
        candidates.extend(matching_bar_phase_candidates(
            outgoing,
            incoming,
            incoming_start,
            target,
            marker_search_window,
            phase_bias,
        ));
        candidates.extend(beat_start_candidates(
            outgoing,
            target,
            marker_search_window,
            phase_bias,
        ));
        if !harmonic_compatibility.is_some_and(|score| score < 0.5)
            && let Some(shorter_window) =
                shorter_transition_search_window(search_window, phase_bias)
        {
            if harmonic_compatibility.is_none_or(|score| score >= 0.5)
                && incoming.downbeat_confidence >= MIN_ACTIONABLE_DOWNBEAT_CONFIDENCE
                && incoming.first_downbeat.is_some()
            {
                candidates.extend(matching_phrase_phase_candidates(
                    outgoing,
                    incoming,
                    incoming_start,
                    target,
                    shorter_window,
                    PhraseLength::FourBars,
                    BarPhaseBias::AtOrAfter,
                ));
            }
            candidates.extend(matching_bar_phase_candidates(
                outgoing,
                incoming,
                incoming_start,
                target,
                shorter_window,
                BarPhaseBias::AtOrAfter,
            ));
            candidates.extend(beat_start_candidates(
                outgoing,
                target,
                shorter_window,
                BarPhaseBias::AtOrAfter,
            ));
        }
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
    if let Ok(index) = candidates.binary_search(&default_start) {
        candidates.remove(index);
    }
    candidates.insert(0, default_start);

    let marker_fallback = beat_aligned.then(|| {
        candidates
            .iter()
            .copied()
            .find(|candidate| trusted_marker_position(outgoing, *candidate))
    });
    let mut best: Option<(Duration, f32)> = None;
    let mut candidates_checked = 0;
    for candidate in candidates.into_iter().take(MAX_ENERGY_START_CANDIDATES) {
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
        if best
            .is_none_or(|(_, best_score)| candidate_score + ENERGY_SELECTION_EPSILON < best_score)
        {
            best = Some((candidate_start, candidate_score));
        }
    }

    let Some((best_start, _)) = best else {
        // A failed energy/phase candidate must never reintroduce the
        // synthetic phase/grid start that the candidate gate rejected.  Keep
        // the first actual trusted marker as the diagnostic raw plan; the
        // quality guard will still select the conservative fallback if that
        // marker is unsafe.
        return (marker_fallback.flatten().unwrap_or(default_start), None);
    };

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
    // BeatMatched candidates must retain an observed trusted outgoing anchor
    // all the way through energy/structure scoring.  A phase duration or
    // global BPM grid is not a substitute for the marker that produced the
    // candidate; rejecting it here prevents a later nearest-marker lookup
    // from silently converting a synthetic start into phase evidence.
    if beat_aligned && marker_anchor_index(outgoing, outgoing_start).is_none() {
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
        incoming_cue_selection: None,
        duration,
        incoming_tempo_ratio,
        harmonic_compatibility,
        incoming_gain,
        tempo_envelope,
        energy_selection: None,
    };
    let quality = evaluate_transition_quality(outgoing, incoming, &plan);
    if quality.issues.iter().any(|issue| {
        issue.blocks_automatic_transition()
            && !matches!(issue, AutoMixQualityIssue::MixEnergyDipTooDeep { .. })
    }) {
        return None;
    }
    let unsafe_energy_penalty = if quality
        .issues
        .iter()
        .any(|issue| matches!(issue, AutoMixQualityIssue::MixEnergyDipTooDeep { .. }))
    {
        1_000.0
    } else {
        // Structural breaks are score-only for an accepted plan, but a
        // candidate whose rendered mix still spends most of its overlap below
        // the analysis floor should not win start selection over a healthy
        // alternative.
        quality
            .min_mix_energy_ratio
            .filter(|ratio| ratio.is_finite() && *ratio < MIN_SAFE_MIX_ENERGY_RATIO)
            .map_or(0.0, |ratio| (MIN_SAFE_MIX_ENERGY_RATIO - ratio) * 100.0)
    };
    let phrase_evidence_penalty = if beat_aligned
        && quality.downbeat_pairs_checked >= MIN_DOWNBEAT_PHASE_PAIRS
        && quality.phrase_pairs_checked == 0
    {
        // Phrase evidence is optional once beat/downbeat evidence is sound,
        // but a candidate with no observed phrase pair should not outrank an
        // otherwise equivalent marker-backed phrase candidate solely because
        // its synthetic phase happens to have a lower energy score.
        1.0
    } else {
        0.0
    };
    transition_start_score(&quality).map(|score| {
        (
            outgoing_start,
            score + unsafe_energy_penalty + phrase_evidence_penalty,
        )
    })
}

fn marker_evidence_penalty(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    quality: &AutoMixQualityReport,
) -> f32 {
    // Keep short but otherwise valid marker overlaps eligible as a final
    // fallback.  Prefer candidates with the corpus-quality evidence budget,
    // however, so a 3-beat tail cannot win over an 8-pair observed overlap.
    let pair_penalty =
        MIN_MARKER_BACKED_KICK_MARKERS.saturating_sub(quality.beat_pairs_checked) as f32 * 0.25;
    let coverage = outgoing
        .trusted_kick_coverage()
        .min(incoming.trusted_kick_coverage());
    let coverage_penalty = (MIN_MARKER_BACKED_KICK_COVERAGE - coverage).max(0.0);
    pair_penalty + coverage_penalty
}

pub fn transition_score_breakdown(quality: &AutoMixQualityReport) -> Option<AutoMixScoreBreakdown> {
    let energy_balance_penalty = energy_balance_score(quality)?;
    let harmonic_clash = harmonic_clash_amount(quality.harmonic_compatibility);
    let vocal_penalty_weight = 1.1 + harmonic_clash * 0.7;
    let vocal_penalty = quality
        .max_dual_vocal_risk
        .filter(|risk| risk.is_finite())
        .map(|risk| (risk / MAX_DUAL_VOCAL_RISK).clamp(0.0, 1.0).powi(2) * vocal_penalty_weight)
        .unwrap_or(0.0);
    let minimum_natural_overlap = if harmonic_clash > 0.0 {
        Duration::from_secs_f32(
            (MIN_NATURAL_MIX_OVERLAP.as_secs_f32() * (1.0 - harmonic_clash * 0.5)).max(2.0),
        )
    } else {
        MIN_NATURAL_MIX_OVERLAP
    };
    let short_mix_penalty = minimum_natural_overlap
        .checked_sub(quality.overlap)
        .map(|shortfall| shortfall.as_secs_f32() * 0.12)
        .unwrap_or(0.0);
    let energy_step_penalty = quality
        .max_mix_energy_step
        .filter(|step| step.is_finite())
        .map(|step| (step - 0.12).max(0.0) * 2.0)
        .unwrap_or(0.0);
    let handoff_energy_penalty = quality
        .handoff_mix_energy_ratio
        .filter(|ratio| ratio.is_finite())
        .map(|ratio| (0.85 - ratio).max(0.0) * 3.0 + (ratio - 1.15).max(0.0) * 2.0)
        .unwrap_or(0.0);
    let handoff_incoming_target = 0.7 + harmonic_clash * 0.1;
    let handoff_ownership_penalty = quality
        .handoff_incoming_mix_share
        .filter(|share| share.is_finite())
        .map(|share| (handoff_incoming_target - share).max(0.0) * (1.5 + harmonic_clash))
        .unwrap_or(0.0);
    let tempo_smoothness_penalty = quality
        .max_tempo_speed_step
        .filter(|step| step.is_finite())
        .map(|step| (step - 0.015).max(0.0) * 8.0)
        .unwrap_or(0.0);
    let phrase_strength_penalty = match quality.phrase_boundary_bars {
        Some(16) => 0.0,
        Some(8) => 0.04,
        Some(4) => 0.10,
        Some(_) => 0.14,
        None => 0.16,
    };
    let structure_usage_penalty = quality
        .structure_overlap_ratio
        .filter(|ratio| ratio.is_finite())
        .map(|ratio| (0.7 - ratio).max(0.0) * 0.12)
        .unwrap_or(0.0);
    let harmonic_overlap_penalty = if harmonic_clash > 0.0 {
        (quality.overlap.as_secs_f32() - 4.0).max(0.0) * 0.04 * harmonic_clash
    } else {
        0.0
    };
    let total = energy_balance_penalty
        + vocal_penalty
        + short_mix_penalty
        + energy_step_penalty
        + handoff_energy_penalty
        + handoff_ownership_penalty
        + tempo_smoothness_penalty
        + phrase_strength_penalty
        + structure_usage_penalty
        + harmonic_overlap_penalty;
    Some(AutoMixScoreBreakdown {
        total,
        energy_balance_penalty,
        vocal_penalty,
        short_mix_penalty,
        energy_step_penalty,
        handoff_energy_penalty,
        handoff_ownership_penalty,
        tempo_smoothness_penalty,
        phrase_strength_penalty,
        structure_usage_penalty,
        harmonic_overlap_penalty,
    })
}

fn transition_start_score(quality: &AutoMixQualityReport) -> Option<f32> {
    transition_score_breakdown(quality).map(|breakdown| breakdown.total)
}

fn harmonic_clash_amount(score: Option<f32>) -> f32 {
    score
        .filter(|score| score.is_finite())
        .map(|score| ((0.5 - score) / 0.5).clamp(0.0, 1.0))
        .unwrap_or(0.0)
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

fn mix_energy_dip_is_blocking(min_ratio: f32) -> bool {
    min_ratio.is_finite() && min_ratio < MIN_SAFE_MIX_ENERGY_RATIO
}

fn has_energy_profile(analysis: &TrackAnalysis) -> bool {
    analysis.energy_profile_rate > 0 && !analysis.energy_profile.is_empty()
}

fn shorter_transition_search_window(
    search_window: Duration,
    phase_bias: BarPhaseBias,
) -> Option<Duration> {
    if !matches!(phase_bias, BarPhaseBias::AtOrBefore) {
        return None;
    }
    let window = MAX_SHORTER_TRANSITION_SEARCH.min(search_window);
    (window >= MIN_AUDIBLE_MIX_OVERLAP).then_some(window)
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
    if outgoing_grid.downbeat_confidence < MIN_ACTIONABLE_DOWNBEAT_CONFIDENCE
        || incoming_grid.downbeat_confidence < MIN_ACTIONABLE_DOWNBEAT_CONFIDENCE
    {
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
    .filter_map(|candidate| {
        valid_outgoing_start(outgoing, candidate)
            .then(|| trusted_marker_near(outgoing, candidate, MAX_BEATMATCH_PHASE_ERROR))
            .flatten()
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
    if outgoing_grid.downbeat_confidence < MIN_ACTIONABLE_DOWNBEAT_CONFIDENCE
        || incoming_grid.downbeat_confidence < MIN_ACTIONABLE_DOWNBEAT_CONFIDENCE
    {
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
    .filter_map(|candidate| {
        valid_outgoing_start(outgoing, candidate)
            .then(|| trusted_marker_near(outgoing, candidate, MAX_BEATMATCH_PHASE_ERROR))
            .flatten()
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
            marker_is_trusted(analysis, *index) && *beat >= earliest && *beat <= latest
        })
        .map(|(_, beat)| beat)
        .filter(|beat| valid_outgoing_start(analysis, *beat))
        .collect::<Vec<_>>();

    if analysis.beat_markers.is_empty()
        && let (Some(first), Some(bpm)) = (analysis.first_beat, analysis.bpm)
        && bpm.is_finite()
        && bpm > 0.0
        && let Some(interval) = beat_interval_from_bpm(bpm)
    {
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

pub fn automix_mix_gains(kind: TransitionKind, progress: f32) -> (f32, f32) {
    match kind {
        TransitionKind::BeatMatched => beat_handoff_mix_gains(progress),
        TransitionKind::Gapless | TransitionKind::Crossfade => equal_power_mix_gains(progress),
    }
}

fn equal_power_mix_gains(progress: f32) -> (f32, f32) {
    let progress = finite_mix_progress(progress);
    let angle = progress * std::f32::consts::FRAC_PI_2;
    (angle.cos().clamp(0.0, 1.0), angle.sin().clamp(0.0, 1.0))
}

fn beat_handoff_mix_gains(progress: f32) -> (f32, f32) {
    let progress = finite_mix_progress(progress);
    let incoming_progress = smoothstep((progress / 0.75).clamp(0.0, 1.0));
    let outgoing_progress = smoothstep(((progress - 0.15) / 0.85).clamp(0.0, 1.0));
    let incoming = (incoming_progress * std::f32::consts::FRAC_PI_2).sin();
    let outgoing = (outgoing_progress * std::f32::consts::FRAC_PI_2).cos();
    let combined_power = outgoing.hypot(incoming);
    let scale = if combined_power > 1.08 {
        1.08 / combined_power
    } else {
        1.0
    };
    (
        (outgoing * scale).clamp(0.0, 1.0),
        (incoming * scale).clamp(0.0, 1.0),
    )
}

fn finite_mix_progress(progress: f32) -> f32 {
    if progress.is_nan() {
        0.0
    } else {
        progress.clamp(0.0, 1.0)
    }
}

fn measured_peak_linear(peak_dbfs: Option<f32>) -> f32 {
    peak_dbfs
        .filter(|peak| peak.is_finite())
        .map(dbfs_to_linear)
        .filter(|peak| peak.is_finite() && *peak >= 0.0)
        .unwrap_or(1.0)
}

fn finite_nonnegative_gain(gain: f32) -> f32 {
    if gain.is_finite() && gain >= 0.0 {
        gain
    } else {
        0.0
    }
}

/// Applies the peak guard to the normal equal-power/DJ curve.
///
/// The returned values are curve gains only; callers still apply their normal
/// deck base gains exactly once. Endpoints are intentionally left untouched,
/// while interior gains are uniformly attenuated only when the predicted
/// same-phase peak exceeds unity.
pub fn automix_peak_safe_mix_gains(
    kind: TransitionKind,
    progress: f32,
    guard: AutoMixPeakGuard,
) -> (f32, f32) {
    let gains = automix_mix_gains(kind, progress);
    let progress = finite_mix_progress(progress);
    if progress <= 0.0 || progress >= 1.0 {
        return gains;
    }

    let outgoing_peak = finite_peak_or_conservative(guard.outgoing_peak);
    let incoming_peak = finite_peak_or_conservative(guard.incoming_peak);
    let predicted_peak = outgoing_peak * gains.0 + incoming_peak * gains.1;
    if predicted_peak.is_nan() || predicted_peak <= 1.0 {
        return gains;
    }
    let scale = if predicted_peak.is_infinite() {
        0.0
    } else {
        (1.0 / predicted_peak).clamp(0.0, 1.0)
    };
    let mut outgoing = gains.0 * scale;
    let mut incoming = gains.1 * scale;

    // The first multiplication can round the weighted result one ulp above
    // unity even though `scale` is exactly `1 / predicted_peak`. Correct only
    // that rounding error, and apply the correction equally to both curve
    // gains so their shape is preserved. A finite number of passes also keeps
    // pathological subnormal inputs from looping forever.
    for _ in 0..4 {
        let guarded_peak = outgoing_peak * outgoing + incoming_peak * incoming;
        if guarded_peak.is_nan() || guarded_peak <= 1.0 {
            break;
        }
        if guarded_peak.is_infinite() {
            outgoing = 0.0;
            incoming = 0.0;
            break;
        }
        let correction = (1.0 / guarded_peak).clamp(0.0, 1.0);
        outgoing *= correction;
        incoming *= correction;
    }
    (outgoing, incoming)
}

fn finite_peak_or_conservative(peak: f32) -> f32 {
    if peak.is_finite() && peak >= 0.0 {
        peak
    } else {
        1.0
    }
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
    let energy = dbfs_to_linear(dbfs);
    energy
        .is_finite()
        .then_some(energy)
        .filter(|energy| *energy >= 0.0)
}

fn windowed_energy_at(
    analysis: &TrackAnalysis,
    position: Duration,
    window: Duration,
) -> Option<f32> {
    if window.is_zero() {
        return energy_at(analysis, position);
    }
    let half_window = window.div_f64(2.0);
    let start = position
        .saturating_sub(half_window)
        .max(analysis.audible_start);
    let end = position
        .saturating_add(half_window)
        .min(analysis.audible_end);
    if end <= start {
        return energy_at(analysis, position);
    }
    let samples = ((end.saturating_sub(start).as_secs_f64()
        * f64::from(analysis.energy_profile_rate))
    .ceil() as u32)
        .clamp(1, 32);
    average_energy_between(analysis, start, end, samples)
}

fn robust_energy_reference(
    analysis: &TrackAnalysis,
    overlap_start: Duration,
    overlap_end: Duration,
) -> Option<f32> {
    if !has_energy_profile(analysis) || overlap_end <= overlap_start {
        return None;
    }
    let start = overlap_start
        .saturating_sub(ENERGY_REFERENCE_RADIUS)
        .max(analysis.audible_start);
    let end = overlap_end
        .saturating_add(ENERGY_REFERENCE_RADIUS)
        .min(analysis.audible_end);
    if end <= start {
        return None;
    }

    let span = end.saturating_sub(start);
    let sample_count = (span.as_secs_f64() * f64::from(analysis.energy_profile_rate))
        .ceil()
        .clamp(1.0, MAX_ENERGY_REFERENCE_SAMPLES as f64) as usize;
    let mut samples = [0.0_f32; MAX_ENERGY_REFERENCE_SAMPLES];
    let mut checked = 0;
    for index in 0..sample_count {
        let position =
            start.saturating_add(span.mul_f64((index as f64 + 0.5) / sample_count as f64));
        if let Some(energy) =
            energy_at(analysis, position).filter(|energy| energy.is_finite() && *energy >= 0.0)
        {
            samples[checked] = energy;
            checked += 1;
        }
    }
    if checked == 0 {
        return None;
    }
    let samples = &mut samples[..checked];
    samples.sort_by(f32::total_cmp);
    let upper_quartile = samples[(checked.saturating_sub(1) * 3) / 4];
    // A 500 ms windowed edge value is itself an aggregate, not a single
    // global spike; retain it when it represents a real available source
    // floor while still using the robust percentile for quiet edges.
    let edge_reference = [
        windowed_energy_at(analysis, overlap_start, MIN_ENERGY_WINDOW),
        windowed_energy_at(analysis, overlap_end, MIN_ENERGY_WINDOW),
    ]
    .into_iter()
    .flatten()
    .filter(|energy| energy.is_finite() && *energy >= 0.0)
    .fold(0.0, f32::max);
    upper_quartile
        .max(edge_reference)
        .is_finite()
        .then_some(upper_quartile.max(edge_reference))
        .filter(|energy| *energy > 1.0e-6)
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
                outgoing
                    .bpm
                    .and_then(beat_interval_from_bpm)
                    .map(|interval| interval.mul_f64(4.0))
                    .unwrap_or(Duration::from_secs(2)),
            )
            .with_phase_segments(&phase_segments)
        })
    });
    (envelope_start, tempo_envelope)
}

fn trusted_structure_overlap(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    incoming_start: Duration,
    maximum: Duration,
) -> Option<Duration> {
    const MINIMUM: Duration = Duration::from_secs(1);
    let duration = trusted_structure_span(outgoing, incoming, incoming_start)?.min(maximum);
    (duration >= MINIMUM).then_some(duration)
}

fn trusted_structure_span(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    incoming_start: Duration,
) -> Option<Duration> {
    const MINIMUM: Duration = Duration::from_secs(1);
    let outgoing_start = outgoing
        .outro_start
        .filter(|_| outgoing.outro_confidence >= 0.65)?;
    let incoming_end = incoming
        .intro_end
        .filter(|_| incoming.intro_confidence >= 0.65)?;
    let outgoing_span = outgoing.audible_end.saturating_sub(outgoing_start);
    let incoming_span = incoming_end.saturating_sub(incoming_start);
    let duration = outgoing_span.min(incoming_span);
    (duration >= MINIMUM).then_some(duration)
}

fn structure_overlap_usage_ratio(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    plan: &TransitionPlan,
) -> Option<f32> {
    if plan.kind == TransitionKind::Gapless {
        return None;
    }
    let span = trusted_structure_span(outgoing, incoming, plan.incoming_start)?;
    if span.is_zero() {
        return None;
    }
    Some((plan.duration.as_secs_f32() / span.as_secs_f32()).clamp(0.0, 1.0))
}

fn safe_incoming_beat_start(incoming: &TrackAnalysis) -> Option<Duration> {
    safe_incoming_beat_starts(incoming).into_iter().next()
}

fn safe_incoming_beat_starts(incoming: &TrackAnalysis) -> Vec<Duration> {
    let mut candidates = Vec::new();
    let pickup_limit = beat_interval(incoming)
        .map(|interval| interval.mul_f64(f64::from(MAX_INCOMING_PICKUP_BEATS)))
        .unwrap_or(MAX_STRUCTURED_INTRO_SKIP)
        .min(MAX_STRUCTURED_INTRO_SKIP);
    // Search a bounded beat window for the first observed, trustworthy cue.
    // Later markers are not automatically safe pickup cuts: they still need
    // the structured-intro or low-energy checks below.
    if let Some(candidate) = incoming_cue_positions_between(
        incoming,
        incoming.audible_start,
        incoming
            .audible_end
            .min(incoming.audible_start.saturating_add(pickup_limit)),
    )
    .into_iter()
    .next()
        && safe_to_skip_initial_pickup(incoming, candidate, pickup_limit)
    {
        candidates.push(candidate);
    }

    if let Some(intro_end) = incoming
        .intro_end
        .filter(|_| incoming.intro_confidence >= 0.65)
    {
        let tolerance = marker_snap_tolerance(incoming)
            .max(MAX_BEATMATCH_PHASE_ERROR)
            .max(Duration::from_millis(120));
        for candidate in incoming_cue_positions_between(
            incoming,
            intro_end.saturating_sub(tolerance),
            incoming
                .audible_end
                .min(intro_end.saturating_add(tolerance)),
        ) {
            if candidates.len() >= MAX_INCOMING_CUE_CANDIDATES {
                break;
            }
            if safe_to_skip_intro_until(incoming, candidate, MAX_STRUCTURED_INTRO_SKIP, 0.55) {
                candidates.push(candidate);
            }
        }
    }

    for candidate in incoming_cue_positions_between(
        incoming,
        incoming.audible_start,
        incoming.audible_end.min(
            incoming
                .audible_start
                .saturating_add(MAX_LOW_ENERGY_INTRO_SKIP),
        ),
    ) {
        if candidates.len() >= MAX_INCOMING_CUE_CANDIDATES {
            break;
        }
        if safe_to_skip_low_energy_intro(incoming, candidate) {
            candidates.push(candidate);
        }
    }

    candidates.sort_unstable();
    candidates.dedup();
    candidates.truncate(MAX_INCOMING_CUE_CANDIDATES);
    candidates
}

fn safe_to_skip_initial_pickup(
    incoming: &TrackAnalysis,
    candidate: Duration,
    maximum_pickup: Duration,
) -> bool {
    if candidate < incoming.audible_start
        || candidate.saturating_sub(incoming.audible_start) > maximum_pickup
    {
        return false;
    }
    let skipped = candidate.saturating_sub(incoming.audible_start);
    // A cue at the audible boundary is not a pickup skip. Keep this tiny
    // numerical snap window metadata-free, but require trusted vocal
    // evidence for every positive skip beyond it (including a sub-beat one).
    if skipped <= MAX_BEATMATCH_PHASE_ERROR {
        return true;
    }
    let vocal_summary = summarize_vocals(incoming);
    if !vocal_summary.known {
        return false;
    }
    if max_vocal_risk_between(incoming, incoming.audible_start, candidate)
        .is_some_and(|risk| risk > MAX_SKIPPED_INTRO_VOCAL_RISK)
    {
        return false;
    }
    if skipped <= beat_interval(incoming).unwrap_or(MIN_ENERGY_WINDOW) {
        return true;
    }
    safe_to_skip_intro_until(incoming, candidate, maximum_pickup, 0.55)
}

fn safe_to_skip_low_energy_intro(incoming: &TrackAnalysis, candidate: Duration) -> bool {
    safe_to_skip_intro_until(incoming, candidate, MAX_LOW_ENERGY_INTRO_SKIP, 0.45)
}

fn safe_to_skip_intro_until(
    incoming: &TrackAnalysis,
    candidate: Duration,
    maximum_skip: Duration,
    max_before_ratio: f32,
) -> bool {
    if candidate <= incoming.audible_start
        || candidate.saturating_sub(incoming.audible_start) > maximum_skip
        || candidate >= incoming.audible_end
    {
        return false;
    }
    let vocal_summary = summarize_vocals(incoming);
    if !vocal_summary.known
        || max_vocal_risk_between(incoming, incoming.audible_start, candidate)
            .is_some_and(|risk| risk > MAX_SKIPPED_INTRO_VOCAL_RISK)
    {
        return false;
    }
    let before = average_energy_between(incoming, incoming.audible_start, candidate, 12);
    let after = average_energy_between(
        incoming,
        candidate,
        incoming
            .audible_end
            .min(candidate.saturating_add(Duration::from_secs(2))),
        8,
    );
    let Some((before, after)) = before.zip(after) else {
        return false;
    };
    before.is_finite() && after.is_finite() && after > 1.0e-6 && before <= after * max_before_ratio
}

fn average_energy_between(
    analysis: &TrackAnalysis,
    start: Duration,
    end: Duration,
    samples: u32,
) -> Option<f32> {
    if end <= start || samples == 0 {
        return None;
    }
    let span = end.saturating_sub(start);
    let mut total = 0.0;
    let mut checked = 0;
    for index in 0..samples {
        let position =
            start.saturating_add(span.mul_f64((f64::from(index) + 0.5) / f64::from(samples)));
        if let Some(energy) = energy_at(analysis, position).filter(|energy| energy.is_finite()) {
            total += energy;
            checked += 1;
        }
    }
    (checked > 0).then_some(total / checked as f32)
}

fn incoming_cue_positions_between(
    analysis: &TrackAnalysis,
    start: Duration,
    end: Duration,
) -> Vec<Duration> {
    if end < start {
        return Vec::new();
    }

    let mut candidates =
        trusted_beat_markers_between_limited(analysis, start, end, MAX_INCOMING_CUE_CANDIDATES);
    let remaining = MAX_INCOMING_CUE_CANDIDATES.saturating_sub(candidates.len());
    if remaining > 0 {
        for downbeat in downbeat_positions_between_limited(analysis, start, end, remaining) {
            if has_trusted_marker_near(analysis, downbeat, MAX_BEATMATCH_PHASE_ERROR) {
                candidates.push(downbeat);
                if candidates.len() >= MAX_INCOMING_CUE_CANDIDATES {
                    break;
                }
            }
        }
    }

    // Do not synthesize an incoming cue from first_beat/BPM.  An audible
    // pickup can only be discarded when the source exposes an observed,
    // individually trusted marker.  Native crossfade planning remains the
    // fallback for markerless analyses.

    candidates.sort_unstable();
    candidates.dedup();
    candidates
}

fn has_trusted_marker_near(
    analysis: &TrackAnalysis,
    position: Duration,
    tolerance: Duration,
) -> bool {
    !analysis.beat_markers.is_empty()
        && trusted_marker_near(analysis, position, tolerance).is_some()
}

fn trusted_marker_near(
    analysis: &TrackAnalysis,
    position: Duration,
    tolerance: Duration,
) -> Option<Duration> {
    analysis
        .beat_markers
        .iter()
        .copied()
        .enumerate()
        .filter(|(index, beat)| {
            marker_is_trusted(analysis, *index) && beat.abs_diff(position) <= tolerance
        })
        .map(|(_, beat)| beat)
        .min_by_key(|beat| beat.abs_diff(position))
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

fn max_vocal_risk_between(analysis: &TrackAnalysis, start: Duration, end: Duration) -> Option<f32> {
    let summary = summarize_vocals(analysis);
    if !summary.known || end <= start {
        return None;
    }
    let rate = f64::from(analysis.vocal_activity_rate);
    let first = (start.as_secs_f64() * rate).floor() as usize;
    let last = (end.as_secs_f64() * rate).ceil() as usize;
    (first..last.min(analysis.vocal_activity.len()))
        .map(|index| effective_vocal_risk(analysis, Duration::from_secs_f64(index as f64 / rate)))
        .reduce(f32::max)
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
        .filter(|(index, _)| marker_is_trusted(analysis, *index))
        .map(|(_, beat)| beat)
        .filter(|beat| *beat >= position)
        .min()
        .filter(|beat| beat.abs_diff(position) <= MAX_SHORTER_TRANSITION_SEARCH)
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
    let Some(interval) = beat_interval_from_bpm(bpm) else {
        return position;
    };
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

fn align_to_strongest_matching_phrase_phase(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    incoming_start: Duration,
    target: Duration,
    search_window: Duration,
    bias: BarPhaseBias,
) -> Option<Duration> {
    [
        PhraseLength::SixteenBars,
        PhraseLength::EightBars,
        PhraseLength::FourBars,
    ]
    .into_iter()
    .find_map(|length| {
        align_to_matching_phrase_phase(
            outgoing,
            incoming,
            incoming_start,
            target,
            search_window,
            length,
            bias,
        )
    })
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
    if outgoing_grid.downbeat_confidence < MIN_ACTIONABLE_DOWNBEAT_CONFIDENCE
        || incoming_grid.downbeat_confidence < MIN_ACTIONABLE_DOWNBEAT_CONFIDENCE
    {
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
        && outgoing.audible_end.saturating_sub(candidate) >= MIN_AUDIBLE_MIX_OVERLAP)
        .then(|| trusted_marker_near(outgoing, candidate, MAX_BEATMATCH_PHASE_ERROR))
        .flatten()
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
    if outgoing_grid.downbeat_confidence < MIN_ACTIONABLE_DOWNBEAT_CONFIDENCE
        || incoming_grid.downbeat_confidence < MIN_ACTIONABLE_DOWNBEAT_CONFIDENCE
    {
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
    (candidate >= outgoing.audible_start && candidate <= outgoing.audible_end)
        .then(|| trusted_marker_near(outgoing, candidate, MAX_BEATMATCH_PHASE_ERROR))
        .flatten()
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
        .filter(|(index, _)| marker_is_trusted(analysis, *index))
        .map(|(_, beat)| beat)
        .filter(|beat| *beat <= position)
        .max()
        .filter(|beat| beat.abs_diff(position) <= MAX_SHORTER_TRANSITION_SEARCH)
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
    let Some(interval) = beat_interval_from_bpm(bpm) else {
        return position;
    };
    let beats = position.saturating_sub(first).as_secs_f64() / interval.as_secs_f64();
    first + interval.mul_f64(beats.floor())
}

fn snap_to_nearest_beat(analysis: &TrackAnalysis, position: Duration) -> Option<Duration> {
    analysis
        .beat_markers
        .iter()
        .copied()
        .enumerate()
        .filter(|(index, _)| marker_is_trusted(analysis, *index))
        .map(|(_, beat)| beat)
        .min_by_key(|beat| beat.abs_diff(position))
        .filter(|beat| beat.abs_diff(position) <= marker_snap_tolerance(analysis))
}

fn marker_snap_tolerance(analysis: &TrackAnalysis) -> Duration {
    analysis
        .bpm
        .and_then(beat_interval_from_bpm)
        .map(|interval| interval.div_f64(4.0))
        .unwrap_or(Duration::from_millis(100))
}

fn marker_confidence(analysis: &TrackAnalysis, index: usize) -> Option<f32> {
    let confidence = analysis.beat_marker_confidences.get(index).copied()?;
    (confidence.is_finite() && (0.0..=1.0).contains(&confidence)).then_some(confidence)
}

fn marker_is_trusted(analysis: &TrackAnalysis, index: usize) -> bool {
    marker_confidence(analysis, index)
        .is_some_and(|confidence| confidence >= MIN_PHASE_MARKER_CONFIDENCE)
}

fn has_trusted_marker_evidence(analysis: &TrackAnalysis) -> bool {
    // A single trusted pickup cue is enough to choose a safe source start,
    // but not enough to establish a local phase anchor for BeatMatched.
    analysis
        .beat_markers
        .iter()
        .enumerate()
        .filter(|(index, _)| marker_is_trusted(analysis, *index))
        .take(MIN_MARKER_BACKED_KICK_MARKERS)
        .count()
        >= MIN_MARKER_BACKED_KICK_MARKERS
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
        incoming_cue_selection: None,
        duration,
        incoming_tempo_ratio: 1.0,
        harmonic_compatibility,
        incoming_gain: 1.0,
        tempo_envelope: None,
        energy_selection: None,
    }
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
    if beat_interval_from_bpm(outgoing_bpm).is_none()
        || beat_interval_from_bpm(incoming_bpm).is_none()
    {
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
    if analysis.beat_confidence.is_finite() && analysis.beat_confidence >= min_beat_confidence {
        return true;
    }
    if analysis.beat_confidence < MIN_MARKER_BACKED_BEAT_CONFIDENCE
        || analysis.beat_markers.len() < MIN_MARKER_BACKED_KICK_MARKERS
        || analysis.beat_marker_confidences.len() != analysis.beat_markers.len()
    {
        return false;
    }

    let trusted = analysis.trusted_kick_coverage();
    trusted.is_finite()
        && trusted * analysis.beat_marker_confidences.len() as f32
            >= MIN_MARKER_BACKED_KICK_MARKERS as f32
        && trusted >= MIN_MARKER_BACKED_KICK_COVERAGE
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
        .filter_map(|(index, beat)| {
            marker_confidence(outgoing, index).map(|confidence| (beat, confidence))
        })
        .collect::<Vec<_>>();
    let incoming_beats = incoming
        .beat_markers
        .iter()
        .copied()
        .enumerate()
        .filter(|(_, beat)| *beat >= incoming_start)
        .filter_map(|(index, beat)| {
            marker_confidence(incoming, index).map(|confidence| (beat, confidence))
        })
        .collect::<Vec<_>>();
    let paired_beats = pair_phase_follow_beats(
        &outgoing_beats,
        &incoming_beats,
        outgoing_start,
        incoming_start,
        global_speed,
        outgoing,
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
    outgoing_analysis: &TrackAnalysis,
    incoming: &TrackAnalysis,
) -> Vec<((Duration, f32), (Duration, f32))> {
    let tolerance = marker_snap_tolerance(incoming).max(MAX_BEATMATCH_PHASE_ERROR);
    let Some(outgoing_anchor) = outgoing_beats.iter().position(|(beat, confidence)| {
        *confidence >= MIN_PHASE_MARKER_CONFIDENCE && beat.abs_diff(outgoing_start) <= tolerance
    }) else {
        return Vec::new();
    };
    let Some(incoming_anchor) = incoming_beats.iter().position(|(beat, confidence)| {
        *confidence >= MIN_PHASE_MARKER_CONFIDENCE && beat.abs_diff(incoming_start) <= tolerance
    }) else {
        return Vec::new();
    };
    let (outgoing_stride, incoming_stride) =
        beat_family_strides_for_speed(outgoing_analysis, incoming, global_speed);
    let mut pairs = Vec::new();
    let mut step = 0_usize;
    while let Some(outgoing_index) =
        outgoing_anchor.checked_add(step.saturating_mul(outgoing_stride))
    {
        let Some(incoming_index) =
            incoming_anchor.checked_add(step.saturating_mul(incoming_stride))
        else {
            break;
        };
        let (Some(outgoing), Some(incoming)) = (
            outgoing_beats.get(outgoing_index),
            incoming_beats.get(incoming_index),
        ) else {
            break;
        };
        let output_elapsed = outgoing.0.saturating_sub(outgoing_start);
        let target_source = incoming_start.saturating_add(Duration::from_secs_f64(
            output_elapsed.as_secs_f64() * f64::from(global_speed),
        ));
        if incoming.0.abs_diff(target_source) > tolerance {
            break;
        }
        pairs.push((*outgoing, *incoming));
        step += 1;
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
            harmonic_compatibility: None,
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
        assert_eq!(incoming.gains_at(start).mid, 0.7);
        assert_eq!(incoming.gains_at(start).high, 0.82);

        let quarter = start + duration / 4;
        assert!((outgoing.gains_at(quarter).low - 0.5).abs() < f32::EPSILON);
        assert!((incoming.gains_at(quarter).low - 0.5).abs() < f32::EPSILON);

        let midpoint = start + duration / 2;
        let outgoing_midpoint = outgoing.gains_at(midpoint);
        assert_eq!(outgoing_midpoint.low, 0.0);
        assert!((outgoing_midpoint.mid - 0.875).abs() < 0.0001);
        assert!((outgoing_midpoint.high - 0.91).abs() < 0.0001);
        let incoming_midpoint = incoming.gains_at(midpoint);
        assert_eq!(incoming_midpoint.low, 1.0);
        assert!((incoming_midpoint.mid - 0.85).abs() < 0.0001);
        assert!((incoming_midpoint.high - 0.91).abs() < 0.0001);
        assert_eq!(
            outgoing.gains_at(start + duration),
            EqGains {
                low: 0.0,
                mid: 0.75,
                high: 0.82,
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
    fn equalizer_transition_ducks_vocal_band_overlap_at_midpoint() {
        let start = Duration::from_secs(10);
        let duration = Duration::from_secs(8);
        let outgoing = EqTransition {
            id: 1,
            source_start: start,
            duration,
            role: EqTransitionRole::Outgoing,
            harmonic_compatibility: None,
        };
        let incoming = EqTransition {
            role: EqTransitionRole::Incoming,
            ..outgoing
        };
        let midpoint = start + duration / 2;

        let outgoing_vocal = vocal_band_gain(outgoing.gains_at(midpoint));
        let incoming_vocal = vocal_band_gain(incoming.gains_at(midpoint));

        assert!(outgoing_vocal < 0.9, "outgoing_vocal={outgoing_vocal}");
        assert!(incoming_vocal < 0.9, "incoming_vocal={incoming_vocal}");
        assert!(
            outgoing_vocal + incoming_vocal < 1.8,
            "outgoing_vocal={outgoing_vocal} incoming_vocal={incoming_vocal}"
        );
    }

    #[test]
    fn equalizer_transition_ducks_presence_more_for_incompatible_keys() {
        let start = Duration::from_secs(10);
        let duration = Duration::from_secs(8);
        let neutral = EqTransition {
            id: 1,
            source_start: start,
            duration,
            role: EqTransitionRole::Outgoing,
            harmonic_compatibility: None,
        };
        let clashing = EqTransition {
            harmonic_compatibility: Some(0.2),
            ..neutral
        };
        let midpoint = start + duration / 2;

        assert!(
            vocal_band_gain(clashing.gains_at(midpoint))
                < vocal_band_gain(neutral.gains_at(midpoint)),
            "neutral={:?} clashing={:?}",
            neutral.gains_at(midpoint),
            clashing.gains_at(midpoint)
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
                harmonic_compatibility: None,
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

    #[test]
    fn beatmatched_mix_curve_moves_the_incoming_deck_forward() {
        assert_eq!(
            automix_mix_gains(TransitionKind::BeatMatched, 0.0),
            (1.0, 0.0)
        );
        let (_, crossfade_incoming) = automix_mix_gains(TransitionKind::Crossfade, 0.5);
        let (_, beatmatched_incoming) = automix_mix_gains(TransitionKind::BeatMatched, 0.5);
        assert!(beatmatched_incoming > crossfade_incoming);

        for step in 0..=4096 {
            let progress = step as f32 / 4096.0;
            let (outgoing, incoming) = automix_mix_gains(TransitionKind::BeatMatched, progress);
            assert!(outgoing.is_finite() && incoming.is_finite());
            assert!((0.0..=1.0).contains(&outgoing));
            assert!((0.0..=1.0).contains(&incoming));
            assert!(
                outgoing.hypot(incoming) <= 1.0801,
                "progress={progress} outgoing={outgoing} incoming={incoming}"
            );
        }
        let (outgoing, incoming) = automix_mix_gains(TransitionKind::BeatMatched, 1.0);
        assert!(outgoing.abs() < 0.0001);
        assert_eq!(incoming, 1.0);
    }

    #[test]
    fn peak_guard_only_ducks_hot_overlaps_and_preserves_normal_curves() {
        let normal = AutoMixPeakGuard::from_sample_peaks(Some(-4.0), Some(-4.0), 1.0, 1.0);
        let near_hot = AutoMixPeakGuard::from_sample_peaks(Some(-3.0), Some(-3.0), 1.0, 1.0);
        let hot = AutoMixPeakGuard::new(1.0, 1.0);
        let asymmetric = AutoMixPeakGuard::new(0.65, 1.0);
        for kind in [
            TransitionKind::Gapless,
            TransitionKind::Crossfade,
            TransitionKind::BeatMatched,
        ] {
            for step in 0..=4096 {
                let progress = step as f32 / 4096.0;
                let raw = automix_mix_gains(kind, progress);
                let safe_normal = automix_peak_safe_mix_gains(kind, progress, normal);
                let safe_near_hot = automix_peak_safe_mix_gains(kind, progress, near_hot);
                let safe_hot = automix_peak_safe_mix_gains(kind, progress, hot);
                assert!((safe_normal.0 - raw.0).abs() < 0.00001);
                assert!((safe_normal.1 - raw.1).abs() < 0.00001);
                assert!(safe_near_hot.0.is_finite() && safe_near_hot.1.is_finite());
                assert!(safe_hot.0.is_finite() && safe_hot.1.is_finite());
                assert!(safe_hot.0 >= 0.0 && safe_hot.1 >= 0.0);
                let hot_peak = hot.outgoing_peak * safe_hot.0 + hot.incoming_peak * safe_hot.1;
                assert!(
                    hot_peak <= 1.0,
                    "hot same-phase peak exceeded unity: kind={kind:?} progress={progress} safe={safe_hot:?} peak={hot_peak}"
                );
                let raw_asymmetric_peak =
                    asymmetric.outgoing_peak * raw.0 + asymmetric.incoming_peak * raw.1;
                let safe_asymmetric = automix_peak_safe_mix_gains(kind, progress, asymmetric);
                let safe_asymmetric_peak = asymmetric.outgoing_peak * safe_asymmetric.0
                    + asymmetric.incoming_peak * safe_asymmetric.1;
                assert!(
                    safe_asymmetric_peak <= 1.0,
                    "asymmetric same-phase peak exceeded unity: kind={kind:?} progress={progress} safe={safe_asymmetric:?} peak={safe_asymmetric_peak}"
                );
                if raw_asymmetric_peak > 1.0 {
                    let outgoing_scale = safe_asymmetric.0 / raw.0;
                    let incoming_scale = safe_asymmetric.1 / raw.1;
                    assert!(
                        (outgoing_scale - incoming_scale).abs() < 0.00001,
                        "asymmetric guard changed curve shape: kind={kind:?} progress={progress} raw={raw:?} safe={safe_asymmetric:?}"
                    );
                }
                if progress > 0.0 && progress < 1.0 {
                    let raw_peak = raw.0 + raw.1;
                    if raw_peak > 1.000001 {
                        assert!(
                            safe_hot.0 < raw.0 || safe_hot.1 < raw.1,
                            "hot overlap was not attenuated: kind={kind:?} progress={progress} raw={raw:?} safe={safe_hot:?}"
                        );
                    }
                }
            }
            assert_eq!(automix_peak_safe_mix_gains(kind, 0.0, hot), (1.0, 0.0));
            assert_eq!(automix_peak_safe_mix_gains(kind, 1.0, hot), (0.0, 1.0));
        }
        let raw_midpoint = automix_mix_gains(TransitionKind::Crossfade, 0.5);
        let guarded_midpoint =
            automix_peak_safe_mix_gains(TransitionKind::Crossfade, 0.5, near_hot);
        assert!(guarded_midpoint.0 < raw_midpoint.0);
        assert!(guarded_midpoint.1 < raw_midpoint.1);
    }

    #[test]
    fn peak_guard_uses_true_peak_after_each_normalization_gain() {
        let mut outgoing = TrackAnalysis::unanalyzed(Duration::from_secs(30));
        outgoing.sample_peak_dbfs = Some(-6.0);
        outgoing.true_peak_dbtp = Some(-1.0);
        let mut incoming = outgoing.clone();
        incoming.true_peak_dbtp = Some(-2.0);

        let guard =
            AutoMixPeakGuard::from_analyses_with_base_gains(&outgoing, &incoming, 0.5, 0.25);

        assert!((guard.outgoing_peak - dbfs_to_linear(-1.0) * 0.5).abs() < 0.0001);
        assert!((guard.incoming_peak - dbfs_to_linear(-2.0) * 0.25).abs() < 0.0001);
    }

    fn analyzed(bpm: f32) -> TrackAnalysis {
        let interval = Duration::from_secs_f32(60.0 / bpm);
        let mut beat_markers = Vec::new();
        let mut beat = Duration::from_secs(1);
        while beat <= Duration::from_secs(179) {
            beat_markers.push(beat);
            beat += interval;
        }
        let marker_count = beat_markers.len();
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
            beat_marker_confidences: vec![0.9; marker_count],
            first_downbeat: Some(Duration::from_secs(1)),
            downbeat_confidence: 0.9,
            musical_key: None,
            rms_dbfs: None,
            sample_peak_dbfs: Some(-4.0),
            integrated_lufs: None,
            true_peak_dbtp: None,
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
    fn beatmatch_prefers_larger_phrase_boundary_when_search_window_allows() {
        let mut outgoing = analyzed(120.0);
        outgoing.duration = Duration::from_secs(188);
        outgoing.audible_end = Duration::from_secs(187);
        let mut beat = *outgoing.beat_markers.last().unwrap();
        while beat < outgoing.audible_end {
            beat += Duration::from_millis(500);
            outgoing.beat_markers.push(beat);
        }
        let incoming = analyzed(120.0);
        let mut config = config();
        config.crossfade = Duration::from_secs(16);

        let plan = plan_transition(&outgoing, &incoming, &config);
        let quality = evaluate_transition_quality(&outgoing, &incoming, &plan);

        assert_eq!(plan.kind, TransitionKind::BeatMatched);
        assert_eq!(plan.outgoing_start, Duration::from_secs(161));
        assert_eq!(plan.outgoing_start + plan.duration, outgoing.audible_end);
        assert_eq!(quality.phrase_boundary_bars, Some(16));

        let mut four_bar_quality = quality.clone();
        four_bar_quality.phrase_boundary_bars = Some(4);
        assert!(
            transition_start_score(&quality).unwrap()
                < transition_start_score(&four_bar_quality).unwrap(),
            "quality={quality:?} four_bar_quality={four_bar_quality:?}"
        );
    }

    #[test]
    fn chooses_beatmatch_for_compatible_confident_tempos() {
        let plan = plan_transition(&analyzed(120.0), &analyzed(124.0), &config());
        assert_eq!(plan.kind, TransitionKind::BeatMatched);
        assert!((plan.incoming_tempo_ratio - 120.0 / 124.0).abs() < 0.0001);
        assert_eq!(plan.duration, Duration::from_secs(10), "plan={plan:?}");
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
        // The fade endpoint is not itself an observed phrase marker; a
        // synthetic global-phase handoff must not be reported as verified.
        assert_eq!(report.handoff_phrase_phase_error, None, "report={report:?}");
        assert!(
            report.low_handoff_min.unwrap() >= 0.99 && report.low_handoff_max.unwrap() <= 1.01,
            "report={report:?}"
        );
        assert!(report.energy_samples_checked > 0, "report={report:?}");
        assert!(
            report.min_mix_energy_ratio.unwrap() >= 0.7,
            "report={report:?}"
        );
        assert!(
            report.max_mix_energy_ratio.unwrap() <= 1.05,
            "report={report:?}"
        );
    }

    #[test]
    fn beat_phase_requires_eight_pairs_and_sufficient_overlap_coverage() {
        let outgoing = analyzed(120.0);
        let incoming = analyzed(120.0);
        let short_plan = TransitionPlan {
            kind: TransitionKind::BeatMatched,
            outgoing_start: Duration::from_secs(1),
            incoming_start: Duration::from_secs(1),
            incoming_cue_selection: None,
            duration: Duration::from_secs(1),
            incoming_tempo_ratio: 1.0,
            harmonic_compatibility: None,
            incoming_gain: 1.0,
            tempo_envelope: None,
            energy_selection: None,
        };
        let short_quality = evaluate_transition_quality(&outgoing, &incoming, &short_plan);
        assert_eq!(short_quality.beat_pairs_checked, 3);
        assert_eq!(short_quality.beat_phase_coverage, Some(1.0));
        assert_eq!(short_quality.max_beat_phase_error, None);
        assert!(
            short_quality
                .issues
                .contains(&AutoMixQualityIssue::BeatPhaseUnverified)
        );

        let full_plan = TransitionPlan {
            duration: Duration::from_secs(4),
            ..short_plan
        };
        let full_quality = evaluate_transition_quality(&outgoing, &incoming, &full_plan);
        assert!(full_quality.beat_pairs_checked >= MIN_BEAT_PHASE_PAIRS);
        assert!(
            full_quality
                .beat_phase_coverage
                .is_some_and(|coverage| coverage >= MIN_BEAT_PHASE_COVERAGE)
        );
        assert!(full_quality.max_beat_phase_error.is_some());
        assert!(
            !full_quality
                .issues
                .contains(&AutoMixQualityIssue::BeatPhaseUnverified)
        );

        let mut sparse_outgoing = outgoing.clone();
        let mut sparse_incoming = incoming.clone();
        for index in [2, 4, 6, 8] {
            sparse_outgoing.beat_marker_confidences[index] = 0.0;
            sparse_incoming.beat_marker_confidences[index] = 0.0;
        }
        let sparse_quality =
            evaluate_transition_quality(&sparse_outgoing, &sparse_incoming, &full_plan);
        assert_eq!(sparse_quality.beat_phase_coverage, Some(0.625));
        assert_eq!(sparse_quality.max_beat_phase_error, None);
        assert!(
            sparse_quality
                .issues
                .contains(&AutoMixQualityIssue::BeatPhaseUnverified)
        );
    }

    #[test]
    fn marker_shifted_outro_uses_local_markers_instead_of_a_synthetic_grid() {
        let mut track = analyzed(131.875);
        track.duration = Duration::from_millis(204_382);
        track.audible_start = Duration::from_millis(236);
        track.audible_end = Duration::from_millis(203_780);
        track.first_beat = Some(Duration::from_millis(250));
        track.first_downbeat = Some(Duration::from_millis(250));
        track.downbeat_confidence = 0.017;
        track.beat_confidence = 0.56;
        track.vocal_activity = vec![0; 205 * 4];
        track.vocal_activity_confidences = vec![255; 205 * 4];
        track.energy_profile = vec![192; 205 * 4];

        let mut markers = Vec::new();
        let mut marker = Duration::from_millis(250);
        while marker <= Duration::from_secs(16) {
            markers.push(marker);
            marker += Duration::from_millis(455);
        }
        marker = Duration::from_millis(194_760);
        while marker <= Duration::from_millis(201_605) {
            markers.push(marker);
            // Keep the local tail's observed phase shift, while retaining
            // the intermediate trusted beat markers needed to measure the
            // full overlap's evidence coverage.
            marker += Duration::from_millis(455);
        }
        track.beat_markers = markers;
        track.beat_marker_confidences = vec![0.75; track.beat_markers.len()];

        let mut config = config();
        config.crossfade = Duration::from_millis(8_345);
        let synthetic_start =
            align_to_global_beat(track.audible_end.saturating_sub(config.crossfade), &track);
        let plan = plan_transition(&track, &track, &config);
        let quality = evaluate_transition_quality(&track, &track, &plan);
        let guarded = plan_guarded_transition(&track, &track, &config);

        assert_eq!(plan.kind, TransitionKind::BeatMatched, "plan={plan:?}");
        assert!(track.beat_markers.contains(&plan.outgoing_start));
        assert_ne!(plan.outgoing_start, synthetic_start);
        assert_eq!(plan.incoming_start, Duration::from_millis(250));
        assert!(quality.beat_pairs_checked >= 5, "quality={quality:?}");
        assert!(
            quality
                .max_beat_phase_error
                .is_some_and(|error| error <= Duration::from_millis(1)),
            "plan={plan:?} quality={quality:?}"
        );
        assert_eq!(quality.handoff_beat_phase_error, None);
        assert!(!quality.has_blocking_issue(), "quality={quality:?}");
        assert_eq!(
            guarded.plan.kind,
            TransitionKind::BeatMatched,
            "{guarded:?}"
        );
    }

    #[test]
    fn low_confidence_downbeats_do_not_override_trusted_beat_alignment() {
        let outgoing = analyzed(120.0);
        let mut incoming = analyzed(120.0);
        incoming.first_downbeat = Some(Duration::from_millis(1_750));
        incoming.downbeat_confidence = 0.4;
        let mut outgoing = outgoing;
        outgoing.downbeat_confidence = 0.4;

        let plan = plan_transition(&outgoing, &incoming, &config());
        let quality = evaluate_transition_quality(&outgoing, &incoming, &plan);

        assert_eq!(plan.kind, TransitionKind::BeatMatched, "plan={plan:?}");
        assert!(quality.beat_pairs_checked > 0, "quality={quality:?}");
        assert_eq!(quality.downbeat_pairs_checked, 0);
        assert_eq!(quality.max_downbeat_phase_error, None);
        assert!(!quality.has_blocking_issue(), "quality={quality:?}");
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
            report
                .issues
                .contains(&AutoMixQualityIssue::DownbeatPhaseUnverified),
            "report={report:?}"
        );
        assert!(report.has_blocking_issue(), "report={report:?}");
    }

    #[test]
    fn marker_ordinal_phase_survives_long_local_tempo_drift() {
        let mut outgoing = analyzed(120.0);
        let mut incoming = analyzed(120.0);
        // Keep the two observed marker streams aligned while allowing their
        // local clock to drift well beyond the global first_downbeat+BPM
        // extrapolation by the end of the track.
        for (index, marker) in outgoing.beat_markers.iter_mut().enumerate() {
            let drift = Duration::from_millis((index as u64 * 180) / 358);
            *marker += drift;
        }
        incoming.beat_markers.clone_from(&outgoing.beat_markers);

        let plan = plan_transition(&outgoing, &incoming, &config());
        let report = evaluate_transition_quality(&outgoing, &incoming, &plan);

        assert_eq!(plan.kind, TransitionKind::BeatMatched, "plan={plan:?}");
        assert!(report.beat_pairs_checked >= 8, "report={report:?}");
        assert!(report.downbeat_pairs_checked >= 2, "report={report:?}");
        assert!(!report.has_blocking_issue(), "report={report:?}");
    }

    #[test]
    fn actionable_phase_metadata_without_observed_phase_pair_fails_closed() {
        let outgoing = analyzed(120.0);
        let mut incoming = analyzed(120.0);
        incoming.first_downbeat = Some(Duration::from_millis(1_250));
        incoming.downbeat_confidence = 0.9;

        let plan = plan_transition(&outgoing, &incoming, &config());
        let report = evaluate_transition_quality(&outgoing, &incoming, &plan);

        assert_eq!(plan.kind, TransitionKind::BeatMatched);
        assert_eq!(report.max_downbeat_phase_error, None);
        assert!(
            report
                .issues
                .contains(&AutoMixQualityIssue::DownbeatPhaseUnverified),
            "report={report:?}"
        );
        assert!(report.has_blocking_issue(), "report={report:?}");
    }

    #[test]
    fn one_sided_marker_confidence_cannot_anchor_a_beatmatch() {
        let mut outgoing = analyzed(120.0);
        outgoing.beat_marker_confidences.clear();
        let incoming = analyzed(120.0);

        let plan = plan_transition(&outgoing, &incoming, &config());

        assert_ne!(plan.kind, TransitionKind::BeatMatched, "plan={plan:?}");
    }

    #[test]
    fn transition_quality_leaves_unobserved_phrase_phase_optional() {
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
        assert!(report.max_phrase_phase_error.is_none(), "report={report:?}");
        assert!(!report.has_blocking_issue(), "report={report:?}");
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
    fn beatmatch_skips_safe_low_energy_intro_to_a_later_safe_downbeat() {
        let outgoing = analyzed(120.0);
        let mut incoming = analyzed(120.0);
        incoming.first_beat = Some(Duration::from_secs(5));
        incoming.first_downbeat = Some(Duration::from_secs(5));
        incoming.beat_markers = (0..350)
            .map(|index| Duration::from_secs_f32(5.0 + index as f32 * 0.5))
            .collect();
        incoming.beat_marker_confidences = vec![1.0; incoming.beat_markers.len()];
        set_energy_ranges(&mut incoming, -14.0, &[(1.0, 5.0, -50.0)]);

        let plan = plan_transition(&outgoing, &incoming, &config());
        let quality = evaluate_transition_quality(&outgoing, &incoming, &plan);

        assert_eq!(plan.kind, TransitionKind::BeatMatched);
        assert!(
            plan.incoming_start >= Duration::from_secs(5),
            "plan={plan:?}"
        );
        assert!(
            plan.incoming_cue_selection
                .is_some_and(|selection| selection.candidates_checked > 1),
            "plan={plan:?}"
        );
        assert!(
            !quality.has_blocking_issue(),
            "plan={plan:?} quality={quality:?}"
        );
    }

    #[test]
    fn safe_incoming_cues_include_later_low_energy_candidates() {
        let mut incoming = analyzed(120.0);
        incoming.first_beat = Some(Duration::from_secs(1));
        incoming.first_downbeat = Some(Duration::from_secs(1));
        incoming.beat_markers = (0..350)
            .map(|index| Duration::from_secs_f32(1.0 + index as f32 * 0.5))
            .collect();
        incoming.beat_marker_confidences = vec![1.0; incoming.beat_markers.len()];
        set_energy_ranges(&mut incoming, -14.0, &[(1.0, 3.0, -50.0)]);

        let candidates = safe_incoming_beat_starts(&incoming);

        assert!(!safe_to_skip_low_energy_intro(
            &incoming,
            Duration::from_secs(1)
        ));
        assert!(safe_to_skip_low_energy_intro(
            &incoming,
            Duration::from_secs(3)
        ));
        assert!(candidates.contains(&Duration::from_secs(1)));
        assert!(candidates.contains(&Duration::from_secs(3)));
        assert!(candidates.len() <= MAX_INCOMING_CUE_CANDIDATES);
    }

    #[test]
    fn beatmatch_skips_safe_low_energy_intro_to_trusted_intro_boundary() {
        let outgoing = analyzed(120.0);
        let mut incoming = analyzed(120.0);
        incoming.intro_end = Some(Duration::from_secs(13));
        incoming.intro_confidence = 0.9;
        incoming.first_beat = Some(Duration::from_secs(1));
        incoming.first_downbeat = Some(Duration::from_secs(1));
        incoming.beat_markers = (0..350)
            .map(|index| Duration::from_secs_f32(1.0 + index as f32 * 0.5))
            .collect();
        incoming.beat_marker_confidences = vec![1.0; incoming.beat_markers.len()];
        set_energy_ranges(&mut incoming, -14.0, &[(1.0, 13.0, -50.0)]);

        let plan = plan_transition(&outgoing, &incoming, &config());
        let quality = evaluate_transition_quality(&outgoing, &incoming, &plan);

        assert_eq!(plan.kind, TransitionKind::BeatMatched);
        assert_eq!(plan.incoming_start, Duration::from_secs(13));
        let selection = plan.incoming_cue_selection.expect("incoming cue selection");
        assert_eq!(selection.default_start, Duration::from_secs(1));
        assert_eq!(selection.selected_start, Duration::from_secs(13));
        assert!(selection.candidates_checked > 1);
        assert!(
            !quality.has_blocking_issue(),
            "plan={plan:?} quality={quality:?}"
        );
    }

    #[test]
    fn structured_intro_skip_keeps_vocal_pickup_intact() {
        let outgoing = analyzed(120.0);
        let mut incoming = analyzed(120.0);
        incoming.intro_end = Some(Duration::from_secs(13));
        incoming.intro_confidence = 0.9;
        incoming.first_beat = Some(Duration::from_secs(13));
        incoming.first_downbeat = Some(Duration::from_secs(13));
        incoming.beat_markers = (0..350)
            .map(|index| Duration::from_secs_f32(13.0 + index as f32 * 0.5))
            .collect();
        incoming.beat_marker_confidences = vec![1.0; incoming.beat_markers.len()];
        set_energy_ranges(&mut incoming, -14.0, &[(1.0, 13.0, -50.0)]);
        set_soft_vocal_ranges(&mut incoming, &[(2.0, 12.0)], 0.3);

        let plan = plan_transition(&outgoing, &incoming, &config());

        assert_eq!(plan.kind, TransitionKind::Crossfade);
        assert_eq!(plan.incoming_start, incoming.audible_start);
    }

    #[test]
    fn low_energy_intro_skip_keeps_vocal_pickup_intact() {
        let outgoing = analyzed(120.0);
        let mut incoming = analyzed(120.0);
        incoming.first_beat = Some(Duration::from_secs(5));
        incoming.first_downbeat = Some(Duration::from_secs(5));
        incoming.beat_markers = (0..350)
            .map(|index| Duration::from_secs_f32(5.0 + index as f32 * 0.5))
            .collect();
        incoming.beat_marker_confidences = vec![1.0; incoming.beat_markers.len()];
        set_energy_ranges(&mut incoming, -14.0, &[(1.0, 5.0, -50.0)]);
        set_soft_vocal_ranges(&mut incoming, &[(1.0, 5.0)], 0.3);

        let plan = plan_transition(&outgoing, &incoming, &config());

        assert_eq!(plan.kind, TransitionKind::Crossfade);
        assert_eq!(plan.incoming_start, incoming.audible_start);
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
    fn guarded_transition_rechecks_an_unsafe_crossfade_fallback_before_using_gapless() {
        let mut outgoing = analyzed(120.0);
        let mut incoming = analyzed(120.0);
        for marker in &mut incoming.beat_markers {
            *marker += Duration::from_millis(250);
        }
        set_energy_ranges(&mut outgoing, -18.0, &[(175.0, 177.0, -42.0)]);
        set_energy_ranges(&mut incoming, -18.0, &[(5.0, 7.0, -42.0)]);
        let config = config();
        let fallback = conservative_crossfade_plan(&outgoing, &incoming, &config);
        let fallback_quality = evaluate_transition_quality(&outgoing, &incoming, &fallback);

        assert_eq!(fallback.kind, TransitionKind::Crossfade);
        assert!(
            fallback_quality.issues.iter().any(|issue| matches!(
                issue,
                AutoMixQualityIssue::MixEnergyDipTooDeep { min_ratio }
                    if *min_ratio < MIN_SAFE_MIX_ENERGY_RATIO
            )),
            "fallback={fallback:?} quality={fallback_quality:?}"
        );

        let guarded = plan_guarded_transition(&outgoing, &incoming, &config);

        assert_eq!(guarded.plan.kind, TransitionKind::Gapless, "{guarded:?}");
        assert!(guarded.quality.is_ok(), "{guarded:?}");
        assert_eq!(
            guarded.rejected_plan.as_ref().map(|plan| plan.kind),
            Some(TransitionKind::BeatMatched)
        );
        assert!(
            guarded
                .rejected_quality
                .as_ref()
                .is_some_and(AutoMixQualityReport::has_blocking_issue),
            "{guarded:?}"
        );
    }

    #[test]
    fn non_beatmatched_guard_skips_beatmatch_and_keeps_a_safe_crossfade() {
        let outgoing = analyzed(120.0);
        let incoming = analyzed(120.0);

        let guarded = plan_guarded_non_beatmatched_transition_with_base_gains(
            &outgoing,
            &incoming,
            &config(),
            1.0,
            1.0,
        );

        assert_eq!(guarded.plan.kind, TransitionKind::Crossfade);
        assert!(!guarded.quality.has_blocking_issue(), "{guarded:?}");
        assert!(guarded.rejected_plan.is_none());
    }

    #[test]
    fn non_beatmatched_guard_uses_gapless_for_an_unsafe_crossfade() {
        let mut outgoing = analyzed(120.0);
        let mut incoming = analyzed(120.0);
        set_energy_ranges(&mut outgoing, -18.0, &[(175.0, 177.0, -42.0)]);
        set_energy_ranges(&mut incoming, -18.0, &[(5.0, 7.0, -42.0)]);

        let guarded = plan_guarded_non_beatmatched_transition_with_base_gains(
            &outgoing,
            &incoming,
            &config(),
            1.0,
            1.0,
        );

        assert_eq!(guarded.plan.kind, TransitionKind::Gapless, "{guarded:?}");
        assert!(guarded.quality.is_ok(), "{guarded:?}");
        assert_eq!(
            guarded.rejected_plan.as_ref().map(|plan| plan.kind),
            Some(TransitionKind::Crossfade)
        );
        assert!(
            guarded
                .rejected_quality
                .as_ref()
                .is_some_and(AutoMixQualityReport::has_blocking_issue),
            "{guarded:?}"
        );
    }

    #[test]
    fn non_beatmatched_guard_is_safe_without_energy_or_finite_gains() {
        let outgoing = TrackAnalysis::unanalyzed(Duration::from_secs(30));
        let incoming = TrackAnalysis::unanalyzed(Duration::from_secs(30));

        let guarded = plan_guarded_non_beatmatched_transition_with_base_gains(
            &outgoing,
            &incoming,
            &config(),
            f32::NAN,
            f32::INFINITY,
        );

        assert_eq!(guarded.plan.kind, TransitionKind::Crossfade);
        assert!(!guarded.quality.has_blocking_issue(), "{guarded:?}");
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
        assert_eq!(plan.duration, Duration::from_secs(4), "plan={plan:?}");
    }

    #[test]
    fn planner_does_not_apply_pairwise_level_or_peak_normalization() {
        let mut outgoing = analyzed(120.0);
        outgoing.rms_dbfs = Some(-18.0);
        let mut incoming = analyzed(120.0);
        incoming.rms_dbfs = Some(-12.0);
        incoming.sample_peak_dbfs = Some(-0.5);
        let plan = plan_transition(&outgoing, &incoming, &config());
        assert_eq!(plan.incoming_gain, 1.0);

        std::mem::swap(&mut outgoing, &mut incoming);
        let plan = plan_transition(&outgoing, &incoming, &config());
        assert_eq!(plan.incoming_gain, 1.0);
    }

    #[test]
    fn normal_peak_measurements_do_not_change_planner_gain() {
        let mut outgoing = analyzed(120.0);
        outgoing.rms_dbfs = Some(-12.0);
        outgoing.sample_peak_dbfs = Some(-2.0);
        let mut incoming = analyzed(120.0);
        incoming.rms_dbfs = Some(-12.0);
        incoming.sample_peak_dbfs = Some(-2.0);

        let plan = plan_transition(&outgoing, &incoming, &config());

        assert_eq!(plan.kind, TransitionKind::BeatMatched);
        assert_eq!(plan.incoming_gain, 1.0, "plan={plan:?}");
    }

    #[test]
    fn hot_peak_measurements_do_not_change_planner_gain() {
        let mut outgoing = analyzed(120.0);
        outgoing.rms_dbfs = Some(-12.0);
        outgoing.sample_peak_dbfs = Some(0.0);
        let mut incoming = analyzed(120.0);
        incoming.rms_dbfs = Some(-12.0);
        incoming.sample_peak_dbfs = Some(0.0);

        let plan = plan_transition(&outgoing, &incoming, &config());

        assert_eq!(plan.kind, TransitionKind::BeatMatched);
        assert_eq!(plan.incoming_gain, 1.0, "plan={plan:?}");
    }

    #[test]
    fn nonfinite_peak_measurements_do_not_poison_transition_gain() {
        let mut outgoing = analyzed(120.0);
        outgoing.rms_dbfs = Some(-12.0);
        outgoing.sample_peak_dbfs = Some(f32::NAN);
        let mut incoming = analyzed(120.0);
        incoming.rms_dbfs = Some(-12.0);
        incoming.sample_peak_dbfs = Some(-2.0);

        let plan = plan_transition(&outgoing, &incoming, &config());

        assert!(plan.incoming_gain.is_finite(), "plan={plan:?}");
        assert!(plan.incoming_gain > 0.0, "plan={plan:?}");
    }

    #[test]
    fn pathological_bpm_does_not_expand_incoming_cue_generation() {
        let mut incoming = analyzed(120.0);
        incoming.bpm = Some(f32::MAX);
        incoming.beat_markers.clear();
        incoming.beat_marker_confidences.clear();
        incoming.first_beat = Some(Duration::from_secs(1));
        incoming.first_downbeat = Some(Duration::from_secs(1));

        let candidates = safe_incoming_beat_starts(&incoming);

        assert!(candidates.is_empty());
        assert!(incoming.beat_grid().is_none());
    }

    #[test]
    fn missing_marker_confidence_does_not_fallback_to_global_beat_confidence() {
        let mut incoming = analyzed(120.0);
        incoming.beat_confidence = 0.8;
        incoming.beat_marker_confidences.clear();

        assert!(safe_incoming_beat_starts(&incoming).is_empty());
        let outgoing = analyzed(120.0);
        let plan = plan_transition(&outgoing, &incoming, &config());
        assert_eq!(plan.kind, TransitionKind::Crossfade, "plan={plan:?}");
        assert_eq!(incoming.trusted_kick_coverage(), 0.0);
    }

    #[test]
    fn markerless_safe_intro_does_not_synthesize_a_beatmatch_cue() {
        let outgoing = analyzed(120.0);
        let mut incoming = analyzed(120.0);
        incoming.first_beat = Some(Duration::from_millis(1_500));
        incoming.first_downbeat = Some(Duration::from_millis(1_500));
        incoming.beat_markers.clear();
        incoming.beat_marker_confidences.clear();
        set_energy_ranges(&mut incoming, -14.0, &[(1.0, 1.5, -50.0)]);

        assert!(safe_incoming_beat_starts(&incoming).is_empty());
        let plan = plan_transition(&outgoing, &incoming, &config());
        assert_eq!(plan.kind, TransitionKind::Crossfade, "plan={plan:?}");
        assert_eq!(plan.incoming_start, incoming.audible_start);
    }

    #[test]
    fn incoming_cue_generation_stops_at_the_candidate_limit() {
        let mut incoming = analyzed(120.0);
        incoming.beat_markers = (0..500)
            .map(|index| Duration::from_millis(1_000 + index))
            .collect();
        incoming.beat_marker_confidences = vec![1.0; incoming.beat_markers.len()];
        set_energy_ranges(&mut incoming, -14.0, &[(1.0, 1.25, -50.0)]);

        let candidates = safe_incoming_beat_starts(&incoming);

        assert_eq!(candidates.len(), MAX_INCOMING_CUE_CANDIDATES);
    }

    #[test]
    fn initial_pickup_search_selects_only_the_first_trusted_marker() {
        let mut incoming = analyzed(120.0);
        incoming.beat_markers = (0..500)
            .map(|index| Duration::from_millis(1_000 + index))
            .collect();
        incoming.beat_marker_confidences = vec![1.0; incoming.beat_markers.len()];

        let candidates = safe_incoming_beat_starts(&incoming);

        assert_eq!(candidates, vec![Duration::from_secs(1)]);
    }

    #[test]
    fn initial_pickup_requires_known_vocals_beyond_the_audible_boundary() {
        let mut incoming = analyzed(120.0);
        let pickup = Duration::from_millis(1_250);
        incoming.first_beat = Some(pickup);
        incoming.first_downbeat = Some(pickup);
        incoming.beat_markers = vec![pickup];
        incoming.beat_marker_confidences = vec![1.0];

        incoming.vocal_activity.clear();
        incoming.vocal_activity_confidences.clear();
        assert!(safe_incoming_beat_starts(&incoming).is_empty());
        assert_eq!(
            explain_beatmatch_decision(
                &analyzed(120.0),
                &incoming,
                &config(),
                &plan_guarded_transition(&analyzed(120.0), &incoming, &config()),
            ),
            AutoMixBeatMatchDecision::NoTrustedIncomingBeatStart
        );

        incoming.vocal_activity = vec![0; 180 * 4];
        incoming.vocal_activity_confidences = vec![0; 180 * 4];
        incoming.vocal_activity_rate = 4;
        assert!(safe_incoming_beat_starts(&incoming).is_empty());

        incoming.vocal_activity_confidences.fill(255);
        assert_eq!(safe_incoming_beat_starts(&incoming), vec![pickup]);
    }

    #[test]
    fn initial_pickup_at_audible_boundary_needs_no_vocal_metadata() {
        let mut incoming = analyzed(120.0);
        incoming.beat_markers = vec![incoming.audible_start];
        incoming.beat_marker_confidences = vec![1.0];
        incoming.first_beat = Some(incoming.audible_start);
        incoming.first_downbeat = Some(incoming.audible_start);
        incoming.vocal_activity.clear();
        incoming.vocal_activity_confidences.clear();
        incoming.vocal_activity_rate = 0;

        assert_eq!(
            safe_incoming_beat_starts(&incoming),
            vec![incoming.audible_start]
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
    fn transition_score_penalizes_abrupt_tempo_speed_steps() {
        let outgoing = analyzed(120.0);
        let incoming = analyzed(120.0);
        let mut smooth_plan = TransitionPlan {
            kind: TransitionKind::Crossfade,
            outgoing_start: Duration::from_secs(171),
            incoming_start: Duration::from_secs(1),
            incoming_cue_selection: None,
            duration: Duration::from_secs(8),
            incoming_tempo_ratio: 1.0,
            harmonic_compatibility: None,
            incoming_gain: 1.0,
            tempo_envelope: Some(TempoEnvelope::new(
                1.0,
                1.0,
                Duration::from_secs(8),
                Duration::ZERO,
            )),
            energy_selection: None,
        };
        let smooth_quality = evaluate_transition_quality(&outgoing, &incoming, &smooth_plan);
        smooth_plan.tempo_envelope = Some(
            TempoEnvelope::new(1.0, 1.04, Duration::from_secs(8), Duration::ZERO)
                .with_phase_segments(&[
                    TempoSegment {
                        output_end: Duration::from_secs(4),
                        speed: 0.96,
                    },
                    TempoSegment {
                        output_end: Duration::from_secs(8),
                        speed: 1.04,
                    },
                ]),
        );
        let abrupt_quality = evaluate_transition_quality(&outgoing, &incoming, &smooth_plan);

        assert_eq!(smooth_quality.max_tempo_speed_step, Some(0.0));
        assert!(
            abrupt_quality.max_tempo_speed_step.unwrap()
                > smooth_quality.max_tempo_speed_step.unwrap(),
            "smooth={smooth_quality:?} abrupt={abrupt_quality:?}"
        );
        assert!(
            transition_start_score(&abrupt_quality).unwrap()
                > transition_start_score(&smooth_quality).unwrap(),
            "smooth={smooth_quality:?} abrupt={abrupt_quality:?}"
        );
    }

    #[test]
    fn transition_score_breakdown_matches_total_score() {
        let outgoing = analyzed(120.0);
        let incoming = analyzed(120.0);
        let plan = TransitionPlan {
            kind: TransitionKind::Crossfade,
            outgoing_start: Duration::from_secs(171),
            incoming_start: Duration::from_secs(1),
            incoming_cue_selection: None,
            duration: Duration::from_secs(8),
            incoming_tempo_ratio: 1.0,
            harmonic_compatibility: None,
            incoming_gain: 1.0,
            tempo_envelope: None,
            energy_selection: None,
        };
        let quality = evaluate_transition_quality(&outgoing, &incoming, &plan);
        let breakdown = transition_score_breakdown(&quality).unwrap();
        let component_sum = [
            breakdown.energy_balance_penalty,
            breakdown.vocal_penalty,
            breakdown.short_mix_penalty,
            breakdown.energy_step_penalty,
            breakdown.handoff_energy_penalty,
            breakdown.handoff_ownership_penalty,
            breakdown.tempo_smoothness_penalty,
            breakdown.phrase_strength_penalty,
            breakdown.structure_usage_penalty,
            breakdown.harmonic_overlap_penalty,
        ]
        .into_iter()
        .sum::<f32>();

        assert!((breakdown.total - component_sum).abs() < f32::EPSILON);
        assert_eq!(
            transition_start_score(&quality).unwrap(),
            breakdown.total,
            "quality={quality:?} breakdown={breakdown:?}"
        );
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

        let quality = evaluate_transition_quality(&outgoing, &incoming, &plan);
        let structure_overlap_ratio = quality
            .structure_overlap_ratio
            .expect("trusted structure overlap should be measured");
        assert!((structure_overlap_ratio - 1.0).abs() < 0.0001);

        let mut low_structure_quality = quality.clone();
        low_structure_quality.structure_overlap_ratio = Some(0.2);
        assert!(
            transition_start_score(&low_structure_quality).unwrap()
                > transition_start_score(&quality).unwrap(),
            "quality={quality:?} low_structure_quality={low_structure_quality:?}"
        );
    }

    #[test]
    fn trusted_first_beat_beyond_the_old_pickup_limit_can_start_a_beatmatch() {
        let outgoing = analyzed(120.0);
        let mut incoming = analyzed(120.0);
        incoming.first_beat = Some(Duration::from_millis(1_250));
        incoming.first_downbeat = Some(Duration::from_millis(1_250));
        incoming.beat_markers = (0..350)
            .map(|index| Duration::from_secs_f32(1.25 + index as f32 * 0.5))
            .collect();

        let plan = plan_transition(&outgoing, &incoming, &config());
        let quality = evaluate_transition_quality(&outgoing, &incoming, &plan);

        assert_eq!(plan.kind, TransitionKind::BeatMatched);
        assert_eq!(plan.incoming_start, Duration::from_millis(1_250));
        assert_eq!(plan.outgoing_start + plan.duration, outgoing.audible_end);
        assert!(
            !quality.has_blocking_issue(),
            "plan={plan:?} quality={quality:?}"
        );
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
            quality.structure_overlap_ratio.unwrap() < 0.5,
            "plan={plan:?} report={quality:?}"
        );
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
            incoming_cue_selection: None,
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
            incoming_cue_selection: None,
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
        assert!(
            report.issues.iter().any(|issue| matches!(
                issue,
                AutoMixQualityIssue::MixEnergyDipTooDeep { min_ratio }
                    if *min_ratio < MIN_SAFE_MIX_ENERGY_RATIO
            )),
            "report={report:?}"
        );
        assert!(report.has_blocking_issue(), "report={report:?}");
    }

    #[test]
    fn transition_energy_uses_nearby_normal_energy_when_edges_are_quiet() {
        let mut outgoing = analyzed(120.0);
        let mut incoming = analyzed(120.0);
        set_energy_ranges(&mut outgoing, -18.0, &[(170.5, 171.5, -42.0)]);
        set_energy_ranges(&mut incoming, -18.0, &[(8.5, 9.5, -42.0)]);
        let plan = TransitionPlan {
            kind: TransitionKind::Crossfade,
            outgoing_start: Duration::from_secs(171),
            incoming_start: Duration::from_secs(1),
            incoming_cue_selection: None,
            duration: Duration::from_secs(8),
            incoming_tempo_ratio: 1.0,
            harmonic_compatibility: None,
            incoming_gain: 1.0,
            tempo_envelope: None,
            energy_selection: None,
        };

        let report = evaluate_transition_quality(&outgoing, &incoming, &plan);

        assert!(report.min_mix_energy_ratio.unwrap() < MIN_SAFE_MIX_ENERGY_RATIO);
        assert!(report.has_blocking_issue(), "report={report:?}");
    }

    #[test]
    fn simultaneous_structural_break_is_score_only() {
        let mut outgoing = analyzed(120.0);
        let mut incoming = analyzed(120.0);
        set_energy_ranges(&mut outgoing, -18.0, &[(175.0, 177.0, -80.0)]);
        set_energy_ranges(&mut incoming, -18.0, &[(5.0, 7.0, -80.0)]);
        let plan = TransitionPlan {
            kind: TransitionKind::Crossfade,
            outgoing_start: Duration::from_secs(171),
            incoming_start: Duration::from_secs(1),
            incoming_cue_selection: None,
            duration: Duration::from_secs(8),
            incoming_tempo_ratio: 1.0,
            harmonic_compatibility: None,
            incoming_gain: 1.0,
            tempo_envelope: None,
            energy_selection: None,
        };

        let report = evaluate_transition_quality(&outgoing, &incoming, &plan);

        assert!(!report.has_blocking_issue(), "report={report:?}");
    }

    #[test]
    fn a_single_windowed_energy_drop_does_not_block_the_transition() {
        let mut outgoing = analyzed(120.0);
        let mut incoming = analyzed(120.0);
        set_energy_ranges(&mut outgoing, -18.0, &[(174.875, 175.125, -80.0)]);
        set_energy_ranges(&mut incoming, -18.0, &[(4.875, 5.125, -80.0)]);
        let plan = TransitionPlan {
            kind: TransitionKind::Crossfade,
            outgoing_start: Duration::from_secs(171),
            incoming_start: Duration::from_secs(1),
            incoming_cue_selection: None,
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
            !report
                .issues
                .iter()
                .any(|issue| matches!(issue, AutoMixQualityIssue::MixEnergyDipTooDeep { .. })),
            "report={report:?}"
        );
    }

    #[test]
    fn normalization_base_gains_are_part_of_energy_quality() {
        let mut outgoing = analyzed(120.0);
        let mut incoming = analyzed(120.0);
        outgoing.duration = Duration::from_secs(10);
        outgoing.audible_end = Duration::from_secs(9);
        incoming.duration = Duration::from_secs(10);
        incoming.audible_end = Duration::from_secs(9);
        set_energy_ranges(&mut outgoing, -6.0, &[]);
        set_energy_ranges(&mut incoming, -18.0, &[]);
        let plan = TransitionPlan {
            kind: TransitionKind::Crossfade,
            outgoing_start: Duration::from_secs(1),
            incoming_start: Duration::from_secs(1),
            incoming_cue_selection: None,
            duration: Duration::from_secs(8),
            incoming_tempo_ratio: 1.0,
            harmonic_compatibility: None,
            incoming_gain: 1.0,
            tempo_envelope: None,
            energy_selection: None,
        };

        let unity = evaluate_transition_quality(&outgoing, &incoming, &plan);
        let normalized =
            evaluate_transition_quality_with_base_gains(&outgoing, &incoming, &plan, 0.25, 1.0);

        assert!(
            unity
                .min_mix_energy_ratio
                .is_some_and(|ratio| (0.24..0.27).contains(&ratio)),
            "unity={unity:?}"
        );
        assert!(!unity.has_blocking_issue(), "unity={unity:?}");
        assert!(
            normalized.min_mix_energy_ratio.unwrap() > 0.65,
            "normalized={normalized:?}"
        );
        assert!(
            !normalized.has_blocking_issue(),
            "normalized={normalized:?}"
        );
    }

    #[test]
    fn healthy_b_like_crossfade_is_kept_above_the_analysis_drop_threshold() {
        let mut outgoing = analyzed(90.0);
        let mut incoming = analyzed(140.0);
        set_energy_ranges(&mut outgoing, -6.0, &[]);
        set_energy_ranges(&mut incoming, -16.5, &[]);

        let guarded =
            plan_guarded_transition_with_base_gains(&outgoing, &incoming, &config(), 0.367, 0.348);

        assert_eq!(guarded.plan.kind, TransitionKind::Crossfade, "{guarded:?}");
        assert!(guarded.rejected_plan.is_none(), "{guarded:?}");
        assert!(guarded.rejected_quality.is_none(), "{guarded:?}");
        let min_ratio = guarded
            .quality
            .min_mix_energy_ratio
            .expect("energy quality ratio");
        assert!((min_ratio - 0.284_295_53).abs() < 0.01, "{guarded:?}");
        assert!(min_ratio < 0.30, "fixture must cover the old cutoff");
        assert!(!guarded.quality.has_blocking_issue(), "{guarded:?}");
    }

    #[test]
    fn analysis_drop_threshold_blocks_only_values_strictly_below_the_boundary() {
        assert!(mix_energy_dip_is_blocking(0.187_062_43));
        assert!(!mix_energy_dip_is_blocking(0.284_295_53));
        assert!(mix_energy_dip_is_blocking(0.199));
        assert!(!mix_energy_dip_is_blocking(MIN_SAFE_MIX_ENERGY_RATIO));
        assert!(!mix_energy_dip_is_blocking(0.201));
        assert!(!mix_energy_dip_is_blocking(f32::NAN));
        assert!(!mix_energy_dip_is_blocking(f32::INFINITY));
    }

    #[test]
    fn planner_finds_a_safe_alternative_to_an_audible_normalized_energy_dip() {
        let mut outgoing = analyzed(120.0);
        let mut incoming = analyzed(120.0);
        outgoing.duration = Duration::from_secs(16);
        outgoing.audible_end = Duration::from_secs(15);
        incoming.duration = Duration::from_secs(16);
        incoming.audible_end = Duration::from_secs(15);
        outgoing
            .beat_markers
            .retain(|marker| *marker <= outgoing.audible_end);
        incoming
            .beat_markers
            .retain(|marker| *marker <= incoming.audible_end);
        set_energy_ranges(&mut outgoing, -12.0, &[(1.25, 8.75, -42.0)]);
        set_energy_ranges(&mut incoming, -18.0, &[(1.25, 8.75, -48.0)]);
        let mut config = config();
        config.crossfade = Duration::from_secs(8);
        let unsafe_plan = TransitionPlan {
            kind: TransitionKind::Crossfade,
            outgoing_start: Duration::from_secs(1),
            incoming_start: Duration::from_secs(1),
            incoming_cue_selection: None,
            duration: Duration::from_secs(8),
            incoming_tempo_ratio: 1.0,
            harmonic_compatibility: None,
            incoming_gain: 1.0,
            tempo_envelope: None,
            energy_selection: None,
        };
        let unsafe_quality = evaluate_transition_quality_with_base_gains(
            &outgoing,
            &incoming,
            &unsafe_plan,
            0.348,
            0.611,
        );

        let guarded =
            plan_guarded_transition_with_base_gains(&outgoing, &incoming, &config, 0.348, 0.611);

        assert!(
            unsafe_quality.has_blocking_issue(),
            "unsafe={unsafe_quality:?}"
        );
        assert!(guarded.plan.duration < unsafe_plan.duration, "{guarded:?}");
        assert!(!guarded.quality.has_blocking_issue(), "{guarded:?}");
        assert!(
            guarded
                .quality
                .min_mix_energy_ratio
                .is_some_and(|ratio| ratio >= MIN_SAFE_MIX_ENERGY_RATIO),
            "{guarded:?}"
        );
    }

    #[test]
    fn energy_score_penalizes_abrupt_mid_mix_level_changes() {
        let smooth_outgoing = analyzed(120.0);
        let smooth_incoming = analyzed(120.0);
        let plan = TransitionPlan {
            kind: TransitionKind::Crossfade,
            outgoing_start: Duration::from_secs(171),
            incoming_start: Duration::from_secs(1),
            incoming_cue_selection: None,
            duration: Duration::from_secs(8),
            incoming_tempo_ratio: 1.0,
            harmonic_compatibility: None,
            incoming_gain: 1.0,
            tempo_envelope: None,
            energy_selection: None,
        };
        let smooth_quality = evaluate_transition_quality(&smooth_outgoing, &smooth_incoming, &plan);

        let mut abrupt_outgoing = analyzed(120.0);
        let abrupt_incoming = analyzed(120.0);
        set_energy_ranges(&mut abrupt_outgoing, -18.0, &[(174.0, 175.0, -3.0)]);
        let abrupt_quality = evaluate_transition_quality(&abrupt_outgoing, &abrupt_incoming, &plan);

        assert!(
            abrupt_quality.max_mix_energy_step.unwrap()
                > smooth_quality.max_mix_energy_step.unwrap(),
            "smooth={smooth_quality:?} abrupt={abrupt_quality:?}"
        );
        assert!(
            transition_start_score(&abrupt_quality).unwrap()
                > transition_start_score(&smooth_quality).unwrap(),
            "smooth={smooth_quality:?} abrupt={abrupt_quality:?}"
        );
    }

    #[test]
    fn energy_score_penalizes_weak_handoff_energy() {
        let mut outgoing = analyzed(120.0);
        let mut matched_incoming = analyzed(120.0);
        let mut weak_incoming = analyzed(120.0);
        set_energy_ranges(&mut outgoing, -18.0, &[]);
        set_energy_ranges(&mut matched_incoming, -18.0, &[]);
        set_energy_ranges(&mut weak_incoming, -30.0, &[]);
        let plan = TransitionPlan {
            kind: TransitionKind::Crossfade,
            outgoing_start: Duration::from_secs(171),
            incoming_start: Duration::from_secs(1),
            incoming_cue_selection: None,
            duration: Duration::from_secs(8),
            incoming_tempo_ratio: 1.0,
            harmonic_compatibility: None,
            incoming_gain: 1.0,
            tempo_envelope: None,
            energy_selection: None,
        };

        let matched_quality = evaluate_transition_quality(&outgoing, &matched_incoming, &plan);
        let weak_quality = evaluate_transition_quality(&outgoing, &weak_incoming, &plan);

        assert!(
            weak_quality.handoff_mix_energy_ratio.unwrap()
                < matched_quality.handoff_mix_energy_ratio.unwrap(),
            "matched={matched_quality:?} weak={weak_quality:?}"
        );
        assert!(
            transition_start_score(&weak_quality).unwrap()
                > transition_start_score(&matched_quality).unwrap(),
            "matched={matched_quality:?} weak={weak_quality:?}"
        );
    }

    #[test]
    fn energy_score_penalizes_outgoing_owned_handoff() {
        let outgoing = analyzed(120.0);
        let incoming = analyzed(120.0);
        let plan = TransitionPlan {
            kind: TransitionKind::Crossfade,
            outgoing_start: Duration::from_secs(171),
            incoming_start: Duration::from_secs(1),
            incoming_cue_selection: None,
            duration: Duration::from_secs(8),
            incoming_tempo_ratio: 1.0,
            harmonic_compatibility: None,
            incoming_gain: 1.0,
            tempo_envelope: None,
            energy_selection: None,
        };
        let quality = evaluate_transition_quality(&outgoing, &incoming, &plan);
        let mut outgoing_owned = quality.clone();
        outgoing_owned.handoff_incoming_mix_share = Some(0.4);

        assert!(
            quality.handoff_incoming_mix_share.unwrap() > 0.7,
            "quality={quality:?}"
        );
        assert!(
            transition_start_score(&outgoing_owned).unwrap()
                > transition_start_score(&quality).unwrap(),
            "quality={quality:?} outgoing_owned={outgoing_owned:?}"
        );
    }

    #[test]
    fn transition_score_weights_vocal_risk_more_when_keys_clash() {
        let outgoing = keyed(0, KeyMode::Major);
        let incoming = keyed(6, KeyMode::Major);
        let plan = TransitionPlan {
            kind: TransitionKind::Crossfade,
            outgoing_start: Duration::from_secs(171),
            incoming_start: Duration::from_secs(1),
            incoming_cue_selection: None,
            duration: Duration::from_secs(4),
            incoming_tempo_ratio: 1.0,
            harmonic_compatibility: Some(1.0),
            incoming_gain: 1.0,
            tempo_envelope: None,
            energy_selection: None,
        };
        let mut compatible_quality = evaluate_transition_quality(&outgoing, &incoming, &plan);
        compatible_quality.max_dual_vocal_risk = Some(0.35);
        let mut clashing_quality = compatible_quality.clone();
        clashing_quality.harmonic_compatibility = Some(0.2);

        assert!(
            transition_start_score(&clashing_quality).unwrap()
                > transition_start_score(&compatible_quality).unwrap(),
            "compatible={compatible_quality:?} clashing={clashing_quality:?}"
        );
    }

    #[test]
    fn transition_score_allows_shorter_overlap_for_key_clashes() {
        let outgoing = keyed(0, KeyMode::Major);
        let incoming = keyed(6, KeyMode::Major);
        let plan = TransitionPlan {
            kind: TransitionKind::Crossfade,
            outgoing_start: Duration::from_secs(176),
            incoming_start: Duration::from_secs(1),
            incoming_cue_selection: None,
            duration: Duration::from_secs(3),
            incoming_tempo_ratio: 1.0,
            harmonic_compatibility: Some(1.0),
            incoming_gain: 1.0,
            tempo_envelope: None,
            energy_selection: None,
        };
        let compatible_quality = evaluate_transition_quality(&outgoing, &incoming, &plan);
        let mut clashing_quality = compatible_quality.clone();
        clashing_quality.harmonic_compatibility = Some(0.2);

        assert!(
            transition_start_score(&clashing_quality).unwrap()
                < transition_start_score(&compatible_quality).unwrap(),
            "compatible={compatible_quality:?} clashing={clashing_quality:?}"
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
            report.min_mix_energy_ratio.unwrap() > 0.55,
            "plan={plan:?} report={report:?}"
        );
        assert!(!report.has_blocking_issue(), "report={report:?}");
    }

    #[test]
    fn energy_start_search_continues_after_an_unverifiable_default() {
        let outgoing = analyzed(120.0);
        let incoming = analyzed(120.0);
        let unverifiable_default = Duration::from_millis(168_750);

        let (selected, selection) = select_energy_balanced_start(
            &outgoing,
            &incoming,
            Duration::from_secs(1),
            unverifiable_default,
            Duration::from_secs(169),
            Duration::from_secs(10),
            true,
            BarPhaseBias::AtOrBefore,
            None,
            1.0,
            Some((1.0, 1.0)),
            config().max_tempo_adjustment,
        );

        let selection = selection.expect("a later marker-backed candidate must be evaluated");
        assert_ne!(selected, unverifiable_default);
        assert!(outgoing.beat_markers.contains(&selected));
        assert!(selection.candidates_checked > 0);
    }

    #[test]
    fn rejected_marker_candidates_do_not_restore_a_synthetic_beatmatch_start() {
        let mut no_valid_outgoing = analyzed(120.0);
        let mut no_valid_incoming = analyzed(120.0);
        // Force every full-length energy candidate through the vocal safety
        // gate while leaving the observed marker stream available for the
        // diagnostic raw plan.  No valid marker candidate means a safe
        // non-beatmatched fallback is correct.
        set_vocal_ranges(&mut no_valid_outgoing, &[(178.0, 179.0)]);
        set_vocal_ranges(&mut no_valid_incoming, &[(1.0, 2.0)]);
        let no_valid_plan = plan_transition(&no_valid_outgoing, &no_valid_incoming, &config());
        assert_ne!(no_valid_plan.kind, TransitionKind::BeatMatched);

        // When the vocal limit leaves one valid marker-backed overlap, that
        // later observed candidate is retained instead of restoring a
        // synthetic default start.
        let mut outgoing = analyzed(120.0);
        let mut incoming = analyzed(120.0);
        set_vocal_ranges(&mut outgoing, &[(176.0, 177.0)]);
        set_vocal_ranges(&mut incoming, &[(1.0, 2.0)]);

        let plan = plan_transition(&outgoing, &incoming, &config());

        assert_eq!(plan.kind, TransitionKind::BeatMatched);
        assert!(trusted_marker_position(&outgoing, plan.outgoing_start));
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
            report.min_mix_energy_ratio.unwrap() > 0.55,
            "plan={plan:?} report={report:?}"
        );
        assert!(!report.has_blocking_issue(), "report={report:?}");
    }

    #[test]
    fn start_selection_avoids_non_blocking_dual_vocal_risk() {
        let mut config = config();
        config.crossfade = Duration::from_secs(16);
        let mut outgoing = analyzed(120.0);
        let mut incoming = analyzed(120.0);
        set_soft_vocal_ranges(&mut outgoing, &[(165.0, 167.0)], 0.5);
        set_soft_vocal_ranges(&mut incoming, &[(5.0, 7.0)], 0.5);

        let default_plan = TransitionPlan {
            kind: TransitionKind::BeatMatched,
            outgoing_start: Duration::from_secs(161),
            incoming_start: incoming.audible_start,
            incoming_cue_selection: None,
            duration: outgoing
                .audible_end
                .saturating_sub(Duration::from_secs(161)),
            incoming_tempo_ratio: 1.0,
            harmonic_compatibility: harmonic_compatibility(&outgoing, &incoming),
            incoming_gain: 1.0,
            tempo_envelope: None,
            energy_selection: None,
        };
        let default_quality = evaluate_transition_quality(&outgoing, &incoming, &default_plan);

        let plan = plan_transition(&outgoing, &incoming, &config);
        let quality = evaluate_transition_quality(&outgoing, &incoming, &plan);

        assert_eq!(plan.kind, TransitionKind::BeatMatched);
        assert!(
            default_quality.max_dual_vocal_risk.unwrap() > quality.max_dual_vocal_risk.unwrap(),
            "default={default_plan:?} default_quality={default_quality:?} plan={plan:?} quality={quality:?}"
        );
        assert!(
            plan.outgoing_start < default_plan.outgoing_start,
            "default={default_plan:?} plan={plan:?}"
        );
        assert!(!quality.has_blocking_issue(), "quality={quality:?}");
    }

    #[test]
    fn start_selection_can_shorten_transition_to_avoid_energy_buildup() {
        let mut outgoing = analyzed(120.0);
        let mut incoming = analyzed(120.0);
        // The early, longer candidates cross a sustained buildup; the later
        // marker-backed candidates have normal energy and can safely shorten
        // the overlap without changing any quality thresholds.
        set_energy_ranges(&mut outgoing, -18.0, &[(169.0, 171.0, -6.0)]);
        set_energy_ranges(&mut incoming, -18.0, &[]);
        let default_start = Duration::from_secs(169);
        let default_plan = TransitionPlan {
            kind: TransitionKind::BeatMatched,
            outgoing_start: default_start,
            incoming_start: incoming.audible_start,
            incoming_cue_selection: None,
            duration: outgoing.audible_end.saturating_sub(default_start),
            incoming_tempo_ratio: 1.0,
            harmonic_compatibility: harmonic_compatibility(&outgoing, &incoming),
            incoming_gain: 1.0,
            tempo_envelope: None,
            energy_selection: None,
        };
        let default_quality = evaluate_transition_quality(&outgoing, &incoming, &default_plan);

        let plan = plan_transition(&outgoing, &incoming, &config());
        let quality = evaluate_transition_quality(&outgoing, &incoming, &plan);

        assert_eq!(plan.kind, TransitionKind::BeatMatched);
        assert!(
            plan.outgoing_start > default_start,
            "default={default_plan:?} plan={plan:?}"
        );
        assert!(
            plan.duration < default_plan.duration,
            "default={default_plan:?} plan={plan:?}"
        );
        assert!(
            quality.max_mix_energy_ratio.unwrap() < default_quality.max_mix_energy_ratio.unwrap(),
            "default_quality={default_quality:?} quality={quality:?}"
        );
        assert!(!quality.has_blocking_issue(), "quality={quality:?}");
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
            incoming_cue_selection: None,
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

    fn set_soft_vocal_ranges(analysis: &mut TrackAnalysis, ranges: &[(f32, f32)], risk: f32) {
        let rate = 4_usize;
        let length = (analysis.duration.as_secs_f32() * rate as f32).ceil() as usize;
        analysis.vocal_activity = vec![0; length];
        analysis.vocal_activity_confidences = vec![255; length];
        analysis.vocal_activity_rate = rate as u8;
        let code = (risk.clamp(0.0, 1.0) * 255.0).round() as u8;
        for (start, end) in ranges {
            let from = (*start * rate as f32).floor() as usize;
            let to = (*end * rate as f32).ceil() as usize;
            analysis.vocal_activity[from..to.min(length)].fill(code);
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
