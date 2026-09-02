//! Serialized, offline Beat This! model access.
//!
//! The model files are compiled into the runtime binary as release assets. No
//! URL or model download path exists in this module. A single `BeatThis` instance
//! is protected by a mutex because
//! rten model execution is mutable; this also keeps lookahead analyses from
//! multiplying the model's working set.

use std::{
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use beat_this::BeatThis;
use sha2::{Digest, Sha256};
use wotoha_core::analysis::{
    AnalysisMethod, AnalysisProvenance, BeatEvent, ComponentProvenance, Confidence, DjCue,
    EnergyAnalysis, KeyMode as V2KeyMode, MeterHypothesis, ModelIdentity, ModelScore, MusicalKey,
    PhraseBoundary, RhythmAnalysis, Section, SectionLabel, StructureAnalysis, Support,
    TempoHypothesis, TempoRelation, TonalAnalysis, TrackAnalysisV2, UnitInterval, VocalAnalysis,
};
use wotoha_core::automix::{KeyMode, MusicalKey as LegacyMusicalKey, PhraseCue, TrackAnalysis};
use wotoha_core::beat_analysis::{
    NeuralBeatObservations, NeuralRhythmAnalysis, TempoRelation as NeuralTempoRelation,
    decode_neural_rhythm, decode_neural_rhythm_with_low_frequency,
};

use crate::embedded_rten::EmbeddedRtenRuntime;

const SMALL_MODEL_SHA256: &str = "a5f8d39d989f31859454ba27afe61c5317ca95e4d9373e6853e5361b8937172f";
const MEL_MODEL_SHA256: &str = "fdd59e65c515331308e4c8841edf99972deca646bdf6197744c2a5b7755e3de9";
const BEAT_THIS_VERSION: &str = "1.0.0";
pub const BEAT_THIS_VCS: &str = "089b509247e6fdcec666511c0dcf0d5f39c21e73";
const SMALL_MODEL_ORIGIN: &str = "https://github.com/danigb/beat-this-rs/blob/089b509247e6fdcec666511c0dcf0d5f39c21e73/models/beat_this_small.onnx";
const MEL_MODEL_ORIGIN: &str = "https://github.com/danigb/beat-this-rs/blob/089b509247e6fdcec666511c0dcf0d5f39c21e73/models/mel_spectrogram.onnx";
const MODEL_LICENSE: &str = "MIT (Beat This! / beat-this-rs)";

/// Beat This! accepts the fixed analysis clock used by the streaming decoder.
/// Keep the direct V2 entry point bounded as well as the production decoder;
/// callers must not be able to bypass the 12-minute neural memory limit by
/// invoking this module directly.
pub const NEURAL_SAMPLE_RATE: u32 = 22_050;
pub const MAX_NEURAL_DURATION: Duration = Duration::from_secs(12 * 60);
pub const MAX_NEURAL_SAMPLES: usize =
    NEURAL_SAMPLE_RATE as usize * MAX_NEURAL_DURATION.as_secs() as usize;
/// Low-band refinement is sampled at the classical 1 kHz clock.  Keep its
/// direct V2 input bounded to the same neural analysis horizon; an empty slice
/// remains valid and simply records zero low-frequency support.
pub const MAX_NEURAL_LOW_BAND_SAMPLES: usize = 1_000 * MAX_NEURAL_DURATION.as_secs() as usize;

static SMALL_MODEL: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/models/beat_this_small.onnx"
));
static MEL_MODEL: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/models/mel_spectrogram.onnx"
));

/// Release-notice metadata for the two offline model assets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BeatModelAssetMetadata {
    pub file_name: &'static str,
    pub sha256: &'static str,
    pub origin: &'static str,
    pub license: &'static str,
}

pub const BEAT_MODEL_ASSETS: [BeatModelAssetMetadata; 2] = [
    BeatModelAssetMetadata {
        file_name: "beat_this_small.onnx",
        sha256: SMALL_MODEL_SHA256,
        origin: SMALL_MODEL_ORIGIN,
        license: MODEL_LICENSE,
    },
    BeatModelAssetMetadata {
        file_name: "mel_spectrogram.onnx",
        sha256: MEL_MODEL_SHA256,
        origin: MEL_MODEL_ORIGIN,
        license: MODEL_LICENSE,
    },
];

type Tracker = Box<dyn FnMut(&[f32], u32) -> Option<NeuralBeatObservations> + Send>;

// `beat-this` intentionally keeps its backend model type private. Store the
// concrete tracker behind a closure so this crate still owns one initialized
// model without depending on an implementation-private type. The mutex is
// initialized independently from the model: a failed load must remain
// retryable, rather than becoming a process-lifetime cached error.
struct TrackerState {
    tracker: Option<Tracker>,
}

static TRACKER: OnceLock<Mutex<TrackerState>> = OnceLock::new();
static INITIALIZATION_COUNT: AtomicUsize = AtomicUsize::new(0);
static ACTIVE_INFERENCE: AtomicUsize = AtomicUsize::new(0);
static MAX_ACTIVE_INFERENCE: AtomicUsize = AtomicUsize::new(0);

struct InferenceGuard;

impl InferenceGuard {
    fn acquire() -> Self {
        let active = ACTIVE_INFERENCE.fetch_add(1, Ordering::AcqRel) + 1;
        MAX_ACTIVE_INFERENCE.fetch_max(active, Ordering::AcqRel);
        Self
    }
}

impl Drop for InferenceGuard {
    fn drop(&mut self) {
        ACTIVE_INFERENCE.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Run the bundled model once and expose its raw activation streams.
#[cfg(test)]
pub(crate) fn analyze(samples: &[f32], sample_rate: u32) -> Option<NeuralBeatObservations> {
    analyze_with_cancel(samples, sample_rate, &AtomicBool::new(false))
}

pub(crate) fn analyze_with_cancel(
    samples: &[f32],
    sample_rate: u32,
    cancelled: &AtomicBool,
) -> Option<NeuralBeatObservations> {
    if sample_rate != NEURAL_SAMPLE_RATE
        || samples.is_empty()
        || samples.len() > MAX_NEURAL_SAMPLES
        || samples.iter().any(|sample| !sample.is_finite())
    {
        return None;
    }
    if cancelled.load(Ordering::Acquire) {
        return None;
    }
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mutex = TRACKER.get_or_init(|| Mutex::new(TrackerState { tracker: None }));
        // A prior panic poisons the mutex, but its state is still safe to
        // inspect and repair. Never turn poisoning into a permanent fallback.
        let mut state = lock_tracker(mutex);
        if state.tracker.is_none() {
            INITIALIZATION_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        if !ensure_tracker(&mut state, build_tracker) {
            return None;
        }
        if cancelled.load(Ordering::Acquire) {
            return None;
        }
        let _inference = InferenceGuard::acquire();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            state
                .tracker
                .as_mut()
                .and_then(|tracker| tracker(samples, sample_rate))
        }));
        match result {
            Ok(result) if !cancelled.load(Ordering::Acquire) => result,
            Ok(_) => None,
            Err(_) => {
                // Do not reuse a tracker whose mutable backend panicked part
                // way through an inference. The next call retries a clean
                // engine while still using this same serialization gate.
                state.tracker = None;
                None
            }
        }
    }))
    .ok()
    .flatten()
}

/// Public, bounded V2 Beat This! entry point.  The returned value is owned by
/// `wotoha-core` and keeps the beat-event timeline intact; callers should use
/// a classical rhythm value when this returns `None`.
pub fn analyze_neural_rhythm(
    samples: &[f32],
    sample_rate: u32,
    low_band_1khz: &[f32],
) -> Option<RhythmAnalysis> {
    let decoded = analyze_neural_rhythm_with_cancel(
        samples,
        sample_rate,
        low_band_1khz,
        &AtomicBool::new(false),
    )?;
    rhythm_analysis_from_neural(decoded)
}

/// Run Beat This! and decode its observations into the event-preserving core
/// rhythm representation.
///
/// The runtime owns model loading, serialization, cancellation and resource
/// limits.  Deterministic DP and low-band timing semantics remain in core. A
/// malformed source rate, oversized input, model initialization failure, or
/// unsupported rhythm returns `None` without mutating any caller-owned
/// classical analysis.
pub(crate) fn analyze_neural_rhythm_with_cancel(
    samples: &[f32],
    sample_rate: u32,
    low_band_1khz: &[f32],
    cancelled: &AtomicBool,
) -> Option<NeuralRhythmAnalysis> {
    if sample_rate != NEURAL_SAMPLE_RATE
        || samples.is_empty()
        || samples.len() > MAX_NEURAL_SAMPLES
        || low_band_1khz.len() > MAX_NEURAL_LOW_BAND_SAMPLES
        || low_band_1khz.iter().any(|sample| !sample.is_finite())
    {
        return None;
    }
    let observations = analyze_with_cancel(samples, sample_rate, cancelled)?;
    if cancelled.load(Ordering::Acquire) {
        return None;
    }
    decode_neural_rhythm_with_low_frequency(&observations, low_band_1khz)
}

/// Decode neural rhythm while preserving an existing classical V2 result on
/// failure. This is intentionally a value-level fallback: no component other
/// than rhythm is replaced or cleared by a failed neural attempt.
pub(crate) fn analyze_neural_rhythm_or_classical<T>(
    samples: &[f32],
    sample_rate: u32,
    low_band_1khz: &[f32],
    classical: T,
    cancelled: &AtomicBool,
) -> Result<RhythmAnalysis, T> {
    let decoded = analyze_neural_rhythm_with_cancel(samples, sample_rate, low_band_1khz, cancelled)
        .and_then(rhythm_analysis_from_neural);
    decoded.ok_or(classical)
}

/// Public non-cancellable fallback boundary for V2 callers that already own
/// the classical rhythm component.  The classical value is moved through on
/// failure, so no clone or mutation of the surrounding track components is
/// required.
pub fn analyze_neural_rhythm_with_fallback(
    samples: &[f32],
    sample_rate: u32,
    low_band_1khz: &[f32],
    classical: RhythmAnalysis,
) -> RhythmAnalysis {
    analyze_neural_rhythm_or_classical(
        samples,
        sample_rate,
        low_band_1khz,
        classical,
        &AtomicBool::new(false),
    )
    .unwrap_or_else(|classical| classical)
}

/// Adapt the legacy runtime aggregate to the versioned analysis shape.
///
/// This adapter is intentionally metadata-only: it never receives PCM and it
/// never reconstructs a beat timeline from a BPM summary.  Existing marker
/// timestamps are copied as V2 events, while missing markers remain missing.
/// The default is the classical V1 interpretation; use
/// [`track_analysis_v2_from_legacy_with_neural_rhythm`] when the caller has
/// just replaced the legacy rhythm fields with a successful Beat This! path.
pub fn track_analysis_v2_from_legacy(analysis: &TrackAnalysis) -> Option<TrackAnalysisV2> {
    track_analysis_v2_from_legacy_with_neural_rhythm(analysis, false)
}

/// Adapter variant used by the production V2 path after a successful neural
/// rhythm decode.  The boolean only controls component provenance; all other
/// V1 components are retained unchanged and are still marked classical.
pub fn track_analysis_v2_from_legacy_with_neural_rhythm(
    analysis: &TrackAnalysis,
    neural_rhythm_succeeded: bool,
) -> Option<TrackAnalysisV2> {
    if analysis.duration.is_zero()
        || analysis.audible_start > analysis.audible_end
        || analysis.audible_end > analysis.duration
        || analysis
            .beat_markers
            .iter()
            .any(|time| *time > analysis.duration)
        || analysis
            .beat_markers
            .windows(2)
            .any(|window| window[0] >= window[1])
    {
        return None;
    }

    let rhythm = legacy_rhythm_to_v2(analysis)?;
    track_analysis_v2_from_legacy_rhythm(analysis, rhythm, neural_rhythm_succeeded)
}

/// Backend-aware form of the adapter used by songbird and cache callers.
/// Only a fresh or cached successful neural analysis can produce Hybrid
/// rhythm provenance; all classical outcomes stay explicitly Classical.
pub fn track_analysis_v2_from_legacy_with_backend(
    analysis: &TrackAnalysis,
    backend: crate::audio_decode::AnalysisBackend,
) -> Option<TrackAnalysisV2> {
    track_analysis_v2_from_legacy_with_neural_rhythm(
        analysis,
        matches!(
            backend,
            crate::audio_decode::AnalysisBackend::Neural
                | crate::audio_decode::AnalysisBackend::CachedNeural
        ),
    )
}

/// Assemble a V2 record from a legacy track and an already-decoded rhythm.
///
/// This is the production bridge: callers can pass the value returned by
/// [`analyze_neural_rhythm`] directly, without first writing it into the V1
/// aggregate.  On a neural failure, pass the unchanged classical rhythm and
/// `false`; every non-rhythm component is still copied from the legacy record.
pub fn track_analysis_v2_from_legacy_rhythm(
    analysis: &TrackAnalysis,
    rhythm: RhythmAnalysis,
    neural_rhythm_succeeded: bool,
) -> Option<TrackAnalysisV2> {
    if analysis.duration.is_zero()
        || analysis.audible_start > analysis.audible_end
        || analysis.audible_end > analysis.duration
        || rhythm
            .beats
            .iter()
            .any(|beat| beat.time > analysis.duration)
        || !rhythm.validate()
    {
        return None;
    }
    let structure = legacy_structure_to_v2(analysis, &rhythm)?;
    let tonal = legacy_tonal_to_v2(analysis.musical_key)?;
    let vocal = legacy_vocal_to_v2(analysis)?;
    let energy = legacy_energy_to_v2(analysis)?;
    let cues = legacy_cues_to_v2(analysis, &rhythm, &structure);
    let provenance = legacy_provenance(analysis, neural_rhythm_succeeded)?;

    TrackAnalysisV2::from_components(
        analysis.duration,
        analysis.audible_start,
        analysis.audible_end,
        rhythm,
        structure,
        tonal,
        vocal,
        energy,
        cues,
        provenance,
    )
}

/// Naming alias for callers that use the domain's `Legacy*Adapter` wording.
pub fn adapt_legacy_track_analysis(analysis: &TrackAnalysis) -> Option<TrackAnalysisV2> {
    track_analysis_v2_from_legacy(analysis)
}

fn legacy_rhythm_to_v2(analysis: &TrackAnalysis) -> Option<RhythmAnalysis> {
    let has_marker_confidences =
        analysis.beat_marker_confidences.len() == analysis.beat_markers.len();
    if analysis
        .beat_marker_confidences
        .iter()
        .any(|confidence| !confidence.is_finite())
        || !analysis.beat_confidence.is_finite()
        || !analysis.downbeat_confidence.is_finite()
    {
        return None;
    }

    let beats = analysis
        .beat_markers
        .iter()
        .copied()
        .enumerate()
        .map(|(index, time)| {
            let marker_confidence =
                has_marker_confidences.then(|| analysis.beat_marker_confidences[index]);
            BeatEvent::new(
                time,
                None,
                None,
                // A legacy marker without a per-marker confidence remains a
                // usable event, but its timing confidence is explicitly
                // neutral rather than inheriting a global summary.
                marker_confidence
                    .map(Confidence::clamped)
                    .unwrap_or_else(|| Confidence::clamped(0.5)),
                None,
                marker_confidence.and_then(Support::new),
            )
        })
        .collect::<Vec<_>>();
    let tempo_hypotheses = analysis
        .bpm
        .filter(|bpm| bpm.is_finite())
        .and_then(|bpm| {
            TempoHypothesis::new(
                bpm,
                UnitInterval::clamped(analysis.beat_confidence),
                TempoRelation::Primary,
            )
        })
        .into_iter()
        .collect::<Vec<_>>();
    let meter_hypotheses = analysis
        .first_downbeat
        .and_then(|downbeat| {
            analysis
                .beat_markers
                .iter()
                .position(|beat| *beat == downbeat)
        })
        .and_then(|phase| {
            (analysis.downbeat_confidence > 0.0).then(|| {
                MeterHypothesis::new(
                    4,
                    phase as u8 % 4,
                    UnitInterval::clamped(analysis.downbeat_confidence),
                )
            })
        })
        .flatten()
        .into_iter()
        .collect::<Vec<_>>();
    RhythmAnalysis::new(beats, tempo_hypotheses, meter_hypotheses)
}

fn legacy_structure_to_v2(
    analysis: &TrackAnalysis,
    rhythm: &RhythmAnalysis,
) -> Option<StructureAnalysis> {
    let beat_times = rhythm
        .beats
        .iter()
        .map(|beat| beat.time)
        .collect::<Vec<_>>();
    let beat_count = beat_times.len();
    if beat_count == 0 {
        return StructureAnalysis::try_new(Vec::new(), Vec::new()).ok();
    }
    let intro_end = analysis
        .intro_end
        .and_then(|time| first_beat_at_or_after(&beat_times, time))
        .filter(|end| *end > 0)
        .unwrap_or(0)
        .min(beat_count);
    let raw_outro_start = analysis
        .outro_start
        .and_then(|time| first_beat_at_or_after(&beat_times, time));
    let outro_start = raw_outro_start
        .map(|start| start.max(intro_end).min(beat_count))
        .unwrap_or(beat_count);

    let mut sections = Vec::with_capacity(3);
    if intro_end > 0 {
        sections.push(Section::new(
            0,
            intro_end,
            bounded_confidence(analysis.intro_confidence),
            [SectionLabel::Intro],
        ));
    }
    // V1 has no section classifier for the body. Keep that interval explicit
    // as Unknown so the adapter does not silently delete the component span.
    if intro_end < outro_start {
        sections.push(Section::new(
            intro_end,
            outro_start,
            0.0,
            [SectionLabel::Unknown],
        ));
    }
    if raw_outro_start.is_some() && outro_start < beat_count {
        sections.push(Section::new(
            outro_start,
            beat_count,
            bounded_confidence(analysis.outro_confidence),
            [SectionLabel::Outro],
        ));
    }
    let phrase_boundaries = analysis
        .phrase_cues()
        .into_iter()
        .filter_map(|phrase| {
            nearest_beat_index(&beat_times, phrase.position).map(|index| {
                PhraseBoundary::periodic_prior(
                    index,
                    UnitInterval::clamped(phrase_strength(phrase)),
                )
            })
        })
        .collect();
    StructureAnalysis::try_new(sections, phrase_boundaries).ok()
}

fn legacy_tonal_to_v2(key: Option<LegacyMusicalKey>) -> Option<TonalAnalysis> {
    let global_key = key.and_then(|key| {
        MusicalKey::new(
            key.tonic,
            match key.mode {
                KeyMode::Major => V2KeyMode::Major,
                KeyMode::Minor => V2KeyMode::Minor,
            },
            Confidence::new(key.confidence)?,
        )
    });
    TonalAnalysis::new(global_key)
}

fn legacy_vocal_to_v2(analysis: &TrackAnalysis) -> Option<VocalAnalysis> {
    if analysis.vocal_activity.is_empty() {
        return Some(VocalAnalysis::default());
    }
    VocalAnalysis::from_quantized(
        &analysis.vocal_activity,
        &analysis.vocal_activity_confidences,
        u16::from(analysis.vocal_activity_rate),
    )
}

fn legacy_energy_to_v2(analysis: &TrackAnalysis) -> Option<EnergyAnalysis> {
    let energy = if analysis.energy_profile.is_empty() {
        EnergyAnalysis::default()
    } else {
        EnergyAnalysis::from_quantized(
            &analysis.energy_profile,
            u16::from(analysis.energy_profile_rate),
        )?
    };
    energy.with_loudness(
        analysis.rms_dbfs,
        analysis.sample_peak_dbfs,
        analysis.integrated_lufs,
        analysis.true_peak_dbtp,
    )
}

fn legacy_cues_to_v2(
    analysis: &TrackAnalysis,
    rhythm: &RhythmAnalysis,
    structure: &StructureAnalysis,
) -> Vec<DjCue> {
    let beat_times = rhythm
        .beats
        .iter()
        .map(|beat| beat.time)
        .collect::<Vec<_>>();
    let mut cues = Vec::new();
    for phrase in analysis.phrase_cues() {
        if let Some(index) = nearest_beat_index(&beat_times, phrase.position) {
            let mut cue = DjCue::new(index, UnitInterval::clamped(phrase_strength(phrase)));
            cue.phrase_boundary = UnitInterval::ONE;
            cue.provenance = wotoha_core::analysis::CueProvenance::PeriodicPrior;
            merge_legacy_cue(&mut cues, cue);
        }
    }
    if let Some(index) = first_beat_at_or_after(&beat_times, analysis.audible_start) {
        let mut cue = DjCue::new(index, UnitInterval::clamped(0.5));
        cue.mix_in = UnitInterval::ONE;
        cue.provenance = wotoha_core::analysis::CueProvenance::Heuristic;
        merge_legacy_cue(&mut cues, cue);
    }
    if let Some(outro) = analysis.outro_start
        && let Some(index) = first_beat_at_or_after(&beat_times, outro)
    {
        let mut cue = DjCue::new(index, UnitInterval::clamped(0.5));
        cue.mix_out = UnitInterval::ONE;
        cue.provenance = wotoha_core::analysis::CueProvenance::Heuristic;
        merge_legacy_cue(&mut cues, cue);
    }
    for section in &structure.sections {
        let mut cue = DjCue::new(
            section.start_beat,
            UnitInterval::from(section.boundary_confidence),
        );
        cue.phrase_boundary = UnitInterval::ONE;
        cue.provenance = wotoha_core::analysis::CueProvenance::Heuristic;
        merge_legacy_cue(&mut cues, cue);
    }
    cues
}

fn legacy_provenance(
    analysis: &TrackAnalysis,
    neural_rhythm_succeeded: bool,
) -> Option<AnalysisProvenance> {
    let rhythm_method = if neural_rhythm_succeeded {
        AnalysisMethod::Hybrid
    } else {
        AnalysisMethod::Classical
    };
    let mut provenance = AnalysisProvenance::new("wotoha-runtime-v2-legacy-adapter")?;
    provenance.schema_version = Some("2".to_owned());
    provenance.rhythm = Some(component("rhythm", rhythm_method, analysis.beat_confidence));
    provenance.structure = Some(component("structure", AnalysisMethod::Classical, 1.0));
    provenance.tonal = Some(component("tonal", AnalysisMethod::Classical, 1.0));
    provenance.vocal = Some(component("vocal", AnalysisMethod::Classical, 1.0));
    provenance.energy = Some(component("energy", AnalysisMethod::Classical, 1.0));
    provenance.cue = Some(component("cue", AnalysisMethod::Derived, 1.0));
    if neural_rhythm_succeeded && let Some(rhythm) = provenance.rhythm.as_mut() {
        rhythm.model = ModelIdentity::new("beat-this", BEAT_THIS_VERSION).map(|mut model| {
            model.revision = Some(BEAT_THIS_VCS.to_owned());
            model
        });
    }
    provenance.overall_confidence = UnitInterval::new(bounded_confidence(analysis.beat_confidence));
    provenance.validate().then_some(provenance)
}

fn component(name: &str, method: AnalysisMethod, confidence: f32) -> ComponentProvenance {
    let mut component = ComponentProvenance::new(name, method).unwrap_or_default();
    component.confidence = Confidence::new(bounded_confidence(confidence));
    component
}

fn bounded_confidence(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn first_beat_at_or_after(beats: &[Duration], time: Duration) -> Option<usize> {
    beats.iter().position(|beat| *beat >= time)
}

fn nearest_beat_index(beats: &[Duration], time: Duration) -> Option<usize> {
    beats
        .iter()
        .enumerate()
        .min_by_key(|(_, beat)| beat.abs_diff(time))
        .map(|(index, _)| index)
}

fn phrase_strength(phrase: PhraseCue) -> f32 {
    match phrase.length.bars() {
        4 => 0.25,
        8 => 0.35,
        16 => 0.45,
        _ => 0.20,
    }
}

fn merge_legacy_cue(cues: &mut Vec<DjCue>, candidate: DjCue) {
    let Some(existing) = cues
        .iter_mut()
        .find(|cue| cue.beat_index == candidate.beat_index)
    else {
        cues.push(candidate);
        return;
    };
    existing.importance = merge_score(existing.importance, candidate.importance);
    existing.mix_in = merge_score(existing.mix_in, candidate.mix_in);
    existing.mix_out = merge_score(existing.mix_out, candidate.mix_out);
    existing.cut_safe = merge_score(existing.cut_safe, candidate.cut_safe);
    existing.phrase_boundary = merge_score(existing.phrase_boundary, candidate.phrase_boundary);
    existing.drop = merge_score(existing.drop, candidate.drop);
    existing.build = merge_score(existing.build, candidate.build);
    if existing.provenance != candidate.provenance {
        existing.provenance = wotoha_core::analysis::CueProvenance::Mixed;
    }
}

fn merge_score(left: UnitInterval, right: UnitInterval) -> UnitInterval {
    UnitInterval::clamped(left.get().max(right.get()))
}

/// Convert the decoder-owned event result into the stable V2 domain type.
///
/// This is the only runtime/core V2 conversion point: Beat This! model output
/// remains in `NeuralBeatObservations`, while all event and score semantics are
/// represented by `wotoha_core::analysis::RhythmAnalysis`.  The converter is
/// deliberately fallible so malformed future decoder output still falls back
/// atomically to the caller's classical rhythm.
pub(crate) fn rhythm_analysis_from_observations(
    observations: &NeuralBeatObservations,
    low_band_1khz: &[f32],
) -> Option<RhythmAnalysis> {
    if low_band_1khz.len() > MAX_NEURAL_LOW_BAND_SAMPLES
        || low_band_1khz.iter().any(|sample| !sample.is_finite())
    {
        return None;
    }
    let decoded = if low_band_1khz.is_empty() {
        // No low-band stream is an unknown support source, not a measured
        // zero. A present but silent stream still travels through the
        // refinement path and is represented as explicit weak support.
        decode_neural_rhythm(observations)
    } else {
        decode_neural_rhythm_with_low_frequency(observations, low_band_1khz)
    };
    decoded.and_then(rhythm_analysis_from_neural)
}

pub(crate) fn rhythm_analysis_from_neural(decoded: NeuralRhythmAnalysis) -> Option<RhythmAnalysis> {
    let NeuralRhythmAnalysis {
        beat_events,
        tempo_hypotheses: neural_tempos,
        first_downbeat_index,
        downbeat_confidence,
        ..
    } = decoded;
    let beats = beat_events
        .into_iter()
        .map(|event| {
            let downbeat_model_score = ModelScore::new(event.downbeat_score);
            Some(BeatEvent::new(
                event.time,
                ModelScore::new(event.model_score),
                downbeat_model_score,
                Confidence::new(event.timing_confidence)?,
                Support::new(event.onset_support),
                event.low_frequency_support.and_then(Support::new),
            ))
        })
        .collect::<Option<Vec<_>>>()?;

    let tempo_hypotheses = neural_tempos
        .into_iter()
        .filter_map(|hypothesis| {
            let relation = match hypothesis.relation {
                NeuralTempoRelation::Primary => TempoRelation::Primary,
                NeuralTempoRelation::HalfTime => TempoRelation::HalfTime,
                NeuralTempoRelation::DoubleTime => TempoRelation::DoubleTime,
                NeuralTempoRelation::Alternative => TempoRelation::Alternative,
            };
            TempoHypothesis::new(
                hypothesis.bpm,
                UnitInterval::new(hypothesis.relative_weight)?,
                relation,
            )
        })
        .collect::<Vec<_>>();

    let meter_hypotheses =
        meter_hypotheses_from_beats(&beats, first_downbeat_index, downbeat_confidence);

    RhythmAnalysis::new(beats, tempo_hypotheses, meter_hypotheses)
}

/// Score the Phase 1 meter candidates directly from the decoded downbeat
/// events. Four-beat is a useful legacy prior, but V2 keeps 2/3/4/6 candidates
/// and allows `resolved_meter()` to return `None` when the evidence is weak or
/// ambiguous.
fn meter_hypotheses_from_beats(
    beats: &[BeatEvent],
    first_downbeat_index: Option<usize>,
    downbeat_confidence: f32,
) -> Vec<MeterHypothesis> {
    if beats.is_empty() {
        return Vec::new();
    }
    let mut hypotheses = Vec::with_capacity(4);
    for beats_per_bar in [2_u8, 3, 4, 6] {
        let mut best: Option<MeterHypothesis> = None;
        for phase in 0..beats_per_bar {
            let mut scores = beats
                .iter()
                .enumerate()
                .filter(|(index, event)| {
                    *index % usize::from(beats_per_bar) == usize::from(phase)
                        && event.downbeat_model_score.is_some()
                })
                .filter_map(|(_, event)| event.downbeat_model_score.map(|score| score.get()))
                .collect::<Vec<_>>();
            if scores.is_empty() {
                continue;
            }
            scores.sort_by(f32::total_cmp);
            // Downbeat evidence is sparse; use the stronger half so one
            // unaccented bar does not erase a stable phase hypothesis.
            let from = scores.len() / 2;
            let score = scores[from..].iter().sum::<f32>() / (scores.len() - from).max(1) as f32;
            let Some(score) = UnitInterval::new(score) else {
                continue;
            };
            let Some(candidate) = MeterHypothesis::new(beats_per_bar, phase, score) else {
                continue;
            };
            if best
                .as_ref()
                .is_none_or(|current| candidate.score.get() > current.score.get())
            {
                best = Some(candidate);
            }
        }
        if let Some(candidate) = best {
            hypotheses.push(candidate);
        }
    }
    // Preserve the decoder's strongest known phase when it carries additional
    // confidence not represented by individual event scores.
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

fn lock_tracker(mutex: &Mutex<TrackerState>) -> std::sync::MutexGuard<'_, TrackerState> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn ensure_tracker(
    state: &mut TrackerState,
    mut builder: impl FnMut() -> Result<Tracker, String>,
) -> bool {
    if state.tracker.is_some() {
        return true;
    }
    let Ok(tracker) = builder() else {
        return false;
    };
    state.tracker = Some(tracker);
    true
}

fn build_tracker() -> Result<Tracker, String> {
    verify_assets()?;
    let runtime = EmbeddedRtenRuntime::new(MEL_MODEL, SMALL_MODEL);
    let mut tracker = BeatThis::new(
        &runtime,
        std::path::Path::new("mel_spectrogram.onnx"),
        std::path::Path::new("beat_this_small.onnx"),
    )
    .map_err(|error| {
        format!("Beat This! {BEAT_THIS_VERSION} model initialization failed: {error}")
    })?;
    let analyzer: Tracker = Box::new(move |samples, sample_rate| {
        let result = tracker.analyze_audio(samples, sample_rate).ok()?;
        if result.beat_logits.len() != result.downbeat_logits.len()
            || result.beat_logits.is_empty()
            || result
                .beat_logits
                .iter()
                .chain(result.downbeat_logits.iter())
                .any(|value| !value.is_finite())
        {
            return None;
        }
        NeuralBeatObservations::new(result.beat_logits, result.downbeat_logits)
    });
    Ok(analyzer)
}

pub fn model_assets() -> &'static [BeatModelAssetMetadata; 2] {
    &BEAT_MODEL_ASSETS
}

fn verify_assets() -> Result<(), String> {
    verify_hash("beat_this_small.onnx", SMALL_MODEL, SMALL_MODEL_SHA256)?;
    verify_hash("mel_spectrogram.onnx", MEL_MODEL, MEL_MODEL_SHA256)
}

fn verify_hash(name: &str, bytes: &[u8], expected: &str) -> Result<(), String> {
    let digest = Sha256::digest(bytes);
    let actual = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "bundled {name} hash mismatch: expected {expected}, got {actual}"
        ))
    }
}

#[cfg(test)]
pub(crate) fn test_inference_counters() -> (usize, usize) {
    (
        INITIALIZATION_COUNT.load(Ordering::Acquire),
        MAX_ACTIVE_INFERENCE.load(Ordering::Acquire),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_assets_have_verified_metadata_and_hashes() {
        verify_assets().expect("bundled model hashes must remain stable");
        assert_eq!(
            BEAT_MODEL_ASSETS[0].license,
            "MIT (Beat This! / beat-this-rs)"
        );
        assert!(
            BEAT_MODEL_ASSETS
                .iter()
                .all(|asset| asset.origin.starts_with("https://"))
        );
    }

    #[test]
    fn initialization_and_model_execution_are_serialized() {
        // Keep this smoke test short; it also protects against accidentally
        // changing the singleton to per-request model construction.
        let samples = vec![0.0_f32; 22_050];
        let _ = analyze(&samples, 22_050);
        let (initializations, max_active) = test_inference_counters();
        assert_eq!(initializations, 1);
        assert_eq!(max_active, 1);
    }

    #[test]
    fn bundled_model_smoke_exposes_finite_raw_logits() {
        let sample_rate = 22_050_u32;
        let mut samples = vec![0.0_f32; sample_rate as usize * 8];
        for beat in 0..16 {
            let start = (0.5 * sample_rate as f32) as usize + beat * (sample_rate as usize / 2);
            for offset in 0..(sample_rate as usize / 40) {
                let index = start + offset;
                if let Some(sample) = samples.get_mut(index) {
                    let phase = offset as f32 / sample_rate as f32 * std::f32::consts::TAU * 80.0;
                    *sample = 0.8 * phase.sin();
                }
            }
        }
        let observations = analyze(&samples, sample_rate).expect("embedded model smoke input");
        assert!(!observations.is_empty());
        assert!(
            observations
                .beat_logits
                .iter()
                .chain(observations.downbeat_logits.iter())
                .all(|value| value.is_finite())
        );
    }

    #[test]
    fn concurrent_requests_share_one_serialized_model() {
        let samples = vec![0.0_f32; 22_050 * 2];
        std::thread::scope(|scope| {
            let first = scope.spawn(|| analyze(&samples, 22_050));
            let second = scope.spawn(|| analyze(&samples, 22_050));
            let _ = first.join();
            let _ = second.join();
        });
        let (initializations, max_active) = test_inference_counters();
        assert!(initializations <= 1);
        assert_eq!(max_active, 1);
    }

    #[test]
    fn failed_initialization_is_retryable_and_success_is_reused() {
        let mut state = TrackerState { tracker: None };
        let mut attempts = 0_u8;
        assert!(!ensure_tracker(&mut state, || {
            attempts += 1;
            Err("injected initialization failure".to_owned())
        }));
        assert!(state.tracker.is_none());
        assert!(ensure_tracker(&mut state, || {
            attempts += 1;
            let tracker: Tracker = Box::new(|_, _| None);
            Ok(tracker)
        }));
        assert_eq!(attempts, 2);
        assert!(ensure_tracker(&mut state, || {
            panic!("a successful tracker must be reused")
        }));
    }

    #[test]
    fn poisoned_tracker_mutex_is_recovered_without_panic() {
        let mutex = Mutex::new(TrackerState { tracker: None });
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = mutex.lock().unwrap();
            panic!("injected tracker mutex poison");
        }));
        assert!(poisoned.is_err());
        let mut state = lock_tracker(&mutex);
        assert!(ensure_tracker(&mut state, || {
            let tracker: Tracker = Box::new(|_, _| None);
            Ok(tracker)
        }));
    }

    #[test]
    fn v2_entry_validates_source_rate_without_touching_the_model() {
        assert!(analyze_neural_rhythm(&[0.0; 8], 44_100, &[]).is_none());
        assert!(analyze_neural_rhythm(&[], NEURAL_SAMPLE_RATE, &[]).is_none());
        assert!(analyze_neural_rhythm(&[0.0; 8], NEURAL_SAMPLE_RATE, &[f32::NAN]).is_none());
        let oversized_low_band = vec![0.0_f32; MAX_NEURAL_LOW_BAND_SAMPLES + 1];
        assert!(
            analyze_neural_rhythm(&[0.0; 8], NEURAL_SAMPLE_RATE, &oversized_low_band).is_none()
        );
    }

    #[test]
    fn fallback_moves_classical_rhythm_through_unchanged() {
        let classical = RhythmAnalysis::from_beats(
            [500, 1_000, 1_500, 2_000]
                .into_iter()
                .map(|millis| BeatEvent::at(Duration::from_millis(millis), Confidence::ONE))
                .collect(),
        )
        .expect("valid classical rhythm");
        let expected = classical.clone();
        let result = analyze_neural_rhythm_with_fallback(&[], NEURAL_SAMPLE_RATE, &[], classical);
        assert_eq!(result, expected);
    }

    #[test]
    fn legacy_adapter_preserves_timeline_components_and_provenance() {
        let mut legacy = TrackAnalysis::unanalyzed(Duration::from_secs(10));
        legacy.audible_start = Duration::from_secs(1);
        legacy.audible_end = Duration::from_secs(10);
        legacy.bpm = Some(120.0);
        legacy.beat_confidence = 0.9;
        legacy.beat_markers = [1, 2, 3, 4, 5, 6, 7, 8]
            .into_iter()
            .map(Duration::from_secs)
            .collect();
        legacy.beat_marker_confidences = vec![0.8; 8];
        legacy.first_beat = legacy.beat_markers.first().copied();
        legacy.first_downbeat = legacy.first_beat;
        legacy.downbeat_confidence = 0.85;
        legacy.intro_end = Some(Duration::from_secs(3));
        legacy.intro_confidence = 0.8;
        legacy.outro_start = Some(Duration::from_secs(8));
        legacy.outro_confidence = 0.8;
        legacy.vocal_activity = vec![0, 128, 255];
        legacy.vocal_activity_confidences = vec![255, 220, 200];
        legacy.vocal_activity_rate = 1;
        legacy.energy_profile = vec![32, 128, 255];
        legacy.energy_profile_rate = 1;
        legacy.rms_dbfs = Some(-14.2);
        legacy.sample_peak_dbfs = Some(-0.8);
        legacy.integrated_lufs = Some(-12.4);
        legacy.true_peak_dbtp = Some(-0.3);
        legacy.musical_key = Some(LegacyMusicalKey {
            tonic: 0,
            mode: KeyMode::Major,
            confidence: 0.75,
        });

        let v2 = track_analysis_v2_from_legacy_with_neural_rhythm(&legacy, true)
            .expect("valid V1 aggregate should adapt");
        assert_eq!(v2.rhythm.beats.len(), legacy.beat_markers.len());
        assert_eq!(v2.rhythm.beats[0].time, legacy.beat_markers[0]);
        assert_eq!(v2.vocal.activity.len(), legacy.vocal_activity.len());
        assert_eq!(v2.energy.profile.len(), legacy.energy_profile.len());
        assert_eq!(v2.energy.rms_dbfs, legacy.rms_dbfs);
        assert_eq!(v2.energy.sample_peak_dbfs, legacy.sample_peak_dbfs);
        assert_eq!(v2.energy.integrated_lufs, legacy.integrated_lufs);
        assert_eq!(v2.energy.true_peak_dbtp, legacy.true_peak_dbtp);
        assert!(v2.structure.sections.len() >= 2);
        assert_eq!(
            v2.provenance
                .rhythm
                .as_ref()
                .map(|component| &component.method),
            Some(&AnalysisMethod::Hybrid)
        );
        assert_eq!(
            v2.provenance
                .cue
                .as_ref()
                .map(|component| &component.method),
            Some(&AnalysisMethod::Derived)
        );
        assert!(v2.validate());
    }

    #[test]
    fn legacy_missing_marker_confidence_stays_neutral_and_unknown() {
        let mut legacy = TrackAnalysis::unanalyzed(Duration::from_secs(4));
        legacy.beat_markers = [0, 1, 2, 3].into_iter().map(Duration::from_secs).collect();
        legacy.bpm = Some(60.0);
        legacy.beat_confidence = 0.95;
        let v2 = track_analysis_v2_from_legacy(&legacy).expect("marker timeline adapts");
        assert!(v2.rhythm.beats.iter().all(|event| {
            event.timing_confidence == Confidence::clamped(0.5)
                && event.onset_support.is_none()
                && event.low_frequency_support.is_none()
        }));
    }
}
