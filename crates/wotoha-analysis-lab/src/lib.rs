//! Isolated, deterministic analysis-accuracy research tooling.
//!
//! This crate owns synthetic fixtures, neutral external observations, result
//! normalization, metrics, and reports.  It is deliberately outside the
//! production playback and AutoMix crates' hot paths.  External DJ software
//! is an observation reference, never ground truth.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt, fs,
    path::Path,
    str::FromStr,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use wotoha_core::{
    analysis::{AnalysisMethod, PhraseBoundarySource, TempoRelation, TrackAnalysisV2},
    audio_analysis::LowBandFilter,
};

pub const LAB_SCHEMA_VERSION: u32 = 1;
pub const OBSERVATION_SCHEMA_VERSION: u32 = 1;
pub const REPORT_SCHEMA_VERSION: u32 = 2;
const DEFAULT_SEED: u64 = 0x57_4f_54_4f_48_41;
const DEFAULT_DURATION_MICROS: u64 = 12_000_000;
const DEFAULT_SAMPLE_RATE: u32 = 22_050;
const MAX_FIXTURES: usize = 256;
const MAX_AUDIO_SAMPLES: usize = DEFAULT_SAMPLE_RATE as usize * 30;
const BEAT_MATCH_WINDOW: Duration = Duration::from_millis(120);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnalyzerMode {
    #[default]
    Hybrid,
    Classical,
}

impl FromStr for AnalyzerMode {
    type Err = LabError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "hybrid" | "native" => Ok(Self::Hybrid),
            "classical" => Ok(Self::Classical),
            _ => Err(LabError::InvalidInput(format!(
                "unknown analyzer mode {value}; expected hybrid or classical"
            ))),
        }
    }
}

#[derive(Debug)]
pub enum LabError {
    Io(std::io::Error),
    Json(serde_json::Error),
    InvalidInput(String),
    HashMismatch {
        sample_id: String,
        expected: String,
        actual: String,
    },
    UnknownSample(String),
    Cancelled,
}

impl fmt::Display for LabError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "I/O error: {error}"),
            Self::Json(error) => write!(f, "JSON error: {error}"),
            Self::InvalidInput(message) => f.write_str(message),
            Self::HashMismatch {
                sample_id,
                expected,
                actual,
            } => write!(
                f,
                "audio SHA-256 mismatch for {sample_id}: expected {expected}, actual {actual}"
            ),
            Self::UnknownSample(sample_id) => write!(
                f,
                "external observation references unknown sample {sample_id}"
            ),
            Self::Cancelled => f.write_str("analysis lab run cancelled"),
        }
    }
}

impl Error for LabError {}
impl From<std::io::Error> for LabError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}
impl From<serde_json::Error> for LabError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), LabError> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_vec_pretty(value)?)?;
    Ok(())
}

pub fn load_manifest(path: &Path) -> Result<SyntheticCorpusManifest, LabError> {
    let manifest: SyntheticCorpusManifest = serde_json::from_slice(&fs::read(path)?)?;
    manifest.validate()?;
    Ok(manifest)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SyntheticCorpusManifest {
    pub schema_version: u32,
    pub split: String,
    pub seed: u64,
    pub fixtures: Vec<FixtureRecord>,
}

impl SyntheticCorpusManifest {
    pub fn validate(&self) -> Result<(), LabError> {
        if self.schema_version != LAB_SCHEMA_VERSION {
            return Err(LabError::InvalidInput(format!(
                "unsupported synthetic corpus schema {}; expected {}",
                self.schema_version, LAB_SCHEMA_VERSION
            )));
        }
        if self.split.trim().is_empty()
            || self.fixtures.is_empty()
            || self.fixtures.len() > MAX_FIXTURES
        {
            return Err(LabError::InvalidInput(
                "synthetic corpus split and bounded fixture list are required".into(),
            ));
        }
        let mut ids = BTreeSet::new();
        for fixture in &self.fixtures {
            if !ids.insert(fixture.spec.id.clone()) {
                return Err(LabError::InvalidInput(format!(
                    "duplicate fixture id {}",
                    fixture.spec.id
                )));
            }
            fixture.spec.validate()?;
            let generated = generate_fixture(&fixture.spec)?;
            if generated.audio_sha256 != fixture.audio_sha256 {
                return Err(LabError::HashMismatch {
                    sample_id: fixture.spec.id.clone(),
                    expected: fixture.audio_sha256.clone(),
                    actual: generated.audio_sha256,
                });
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FixtureRecord {
    pub spec: FixtureSpec,
    pub truth: AnalysisGroundTruth,
    pub audio_sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FixtureSpec {
    pub id: String,
    pub family: FixtureFamily,
    pub duration_micros: u64,
    pub sample_rate: u32,
    pub channels: u8,
    pub lead_in_micros: u64,
    pub meter: u8,
    pub tempo: TempoProfile,
    pub event_style: EventStyle,
    pub transform: TransformKind,
    pub base_id: Option<String>,
    pub seed: u64,
}

impl FixtureSpec {
    fn validate(&self) -> Result<(), LabError> {
        if self.id.trim().is_empty()
            || self.duration_micros == 0
            || self.duration_micros > 30_000_000
            || self.sample_rate == 0
            || self.channels == 0
            || !matches!(self.meter, 2 | 3 | 4 | 6)
        {
            return Err(LabError::InvalidInput(format!(
                "invalid fixture specification {}",
                self.id
            )));
        }
        if self.lead_in_micros >= self.duration_micros {
            return Err(LabError::InvalidInput(format!(
                "fixture {} lead-in exceeds duration",
                self.id
            )));
        }
        self.tempo.validate()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FixtureFamily {
    ConstantTempo,
    HalfDouble,
    MissingBeat,
    ExtraTransient,
    TempoDrift,
    Percussion,
    DownbeatAmbiguity,
    Meter,
    Transform,
}

impl FixtureFamily {
    fn as_str(&self) -> &'static str {
        match self {
            Self::ConstantTempo => "constant_tempo",
            Self::HalfDouble => "half_double",
            Self::MissingBeat => "missing_beat",
            Self::ExtraTransient => "extra_transient",
            Self::TempoDrift => "tempo_drift",
            Self::Percussion => "percussion",
            Self::DownbeatAmbiguity => "downbeat_ambiguity",
            Self::Meter => "meter",
            Self::Transform => "transform",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum TempoProfile {
    Constant {
        bpm: f32,
    },
    LinearRamp {
        start_bpm: f32,
        end_bpm: f32,
    },
    StepReturn {
        base_bpm: f32,
        step_bpm: f32,
        start_micros: u64,
        end_micros: u64,
    },
}

impl TempoProfile {
    fn validate(&self) -> Result<(), LabError> {
        let valid = match self {
            Self::Constant { bpm } => *bpm > 0.0 && bpm.is_finite(),
            Self::LinearRamp { start_bpm, end_bpm } => {
                *start_bpm > 0.0 && *end_bpm > 0.0 && start_bpm.is_finite() && end_bpm.is_finite()
            }
            Self::StepReturn {
                base_bpm,
                step_bpm,
                start_micros,
                end_micros,
            } => {
                *base_bpm > 0.0
                    && base_bpm.is_finite()
                    && step_bpm.is_finite()
                    && start_micros < end_micros
            }
        };
        valid
            .then_some(())
            .ok_or_else(|| LabError::InvalidInput("invalid tempo profile".into()))
    }

    fn bpm_at(&self, time_micros: u64, duration_micros: u64) -> f32 {
        match self {
            Self::Constant { bpm } => *bpm,
            Self::LinearRamp { start_bpm, end_bpm } => {
                let fraction = time_micros as f32 / duration_micros.max(1) as f32;
                start_bpm + (end_bpm - start_bpm) * fraction.clamp(0.0, 1.0)
            }
            Self::StepReturn {
                base_bpm,
                step_bpm,
                start_micros,
                end_micros,
            } => {
                if (*start_micros..*end_micros).contains(&time_micros) {
                    base_bpm + step_bpm
                } else {
                    *base_bpm
                }
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventStyle {
    Standard,
    StrongEverySecondBeat,
    WeakSubdivision,
    KickOnly,
    SnareOnly,
    HatsOnly,
    AttenuatedKick,
    KickRemoved,
    SyncopatedKick,
    ClearDownbeat,
    NoBeatOneKick,
    DisplacedAccent,
    Pickup,
    BreakdownReentry,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransformKind {
    None,
    Gain { factor: f32 },
    Compression,
    Eq,
    HighPass,
    LowPass,
    Mono,
    Stereo,
    SampleRate { sample_rate: u32 },
}

impl TransformKind {
    fn label(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Gain { .. } => "gain",
            Self::Compression => "compression",
            Self::Eq => "eq",
            Self::HighPass => "high_pass",
            Self::LowPass => "low_pass",
            Self::Mono => "mono",
            Self::Stereo => "stereo",
            Self::SampleRate { .. } => "sample_rate",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AnalysisGroundTruth {
    pub tempo: Option<TempoTruth>,
    pub meter: Option<u8>,
    pub beat_times_micros: Vec<u64>,
    pub downbeats: Vec<usize>,
    pub tempo_segments: Vec<TempoSegmentTruth>,
    pub duration_micros: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TempoTruth {
    pub primary_bpm: f32,
    pub valid_alternates_bpm: Vec<f32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TempoSegmentTruth {
    pub start_micros: u64,
    pub end_micros: u64,
    pub start_bpm: f32,
    pub end_bpm: f32,
}

#[derive(Clone, Debug)]
pub struct SyntheticFixture {
    pub spec: FixtureSpec,
    pub audio: Vec<f32>,
    pub truth: AnalysisGroundTruth,
    pub audio_sha256: String,
}

pub fn generate_default_manifest(seed: u64) -> Result<SyntheticCorpusManifest, LabError> {
    let seed = if seed == 0 { DEFAULT_SEED } else { seed };
    let mut specs = Vec::new();
    let bpms: [f32; 14] = [
        60.0, 70.0, 80.0, 90.0, 100.0, 110.0, 120.0, 127.5, 128.0, 130.0, 140.0, 150.0, 160.0,
        180.0,
    ];
    for (index, bpm) in bpms.into_iter().enumerate() {
        let lead_in_micros = if (bpm - 128.0).abs() < f32::EPSILON {
            0
        } else {
            index as u64 * 7_000
        };
        specs.push(spec(
            format!("constant-{bpm:.1}"),
            FixtureFamily::ConstantTempo,
            TempoProfile::Constant { bpm },
            EventStyle::Standard,
            4,
            lead_in_micros,
            seed,
        ));
    }
    for (id, style) in [
        (
            "half-strong-every-second",
            EventStyle::StrongEverySecondBeat,
        ),
        ("half-weak-subdivision", EventStyle::WeakSubdivision),
        ("double-hat-grid", EventStyle::HatsOnly),
        ("halftime-drum-feel", EventStyle::StrongEverySecondBeat),
    ] {
        specs.push(spec(
            id,
            FixtureFamily::HalfDouble,
            TempoProfile::Constant { bpm: 128.0 },
            style,
            4,
            400_000,
            seed,
        ));
    }
    for (id, offset) in [("missing-one-beat", 4), ("missing-two-beats", 9)] {
        specs.push(spec(
            id,
            FixtureFamily::MissingBeat,
            TempoProfile::Constant { bpm: 120.0 },
            EventStyle::Standard,
            4,
            500_000,
            seed + offset,
        ));
    }
    for id in ["extra-off-grid-click", "extra-drum-fill"] {
        specs.push(spec(
            id,
            FixtureFamily::ExtraTransient,
            TempoProfile::Constant { bpm: 127.5 },
            EventStyle::Standard,
            4,
            750_000,
            seed,
        ));
    }
    for (index, percent) in [0.1, 0.25, 0.5, 1.0, 2.0, 4.0].into_iter().enumerate() {
        specs.push(spec(
            format!("ramp-plus-{percent:.2}pct"),
            FixtureFamily::TempoDrift,
            TempoProfile::LinearRamp {
                start_bpm: 120.0,
                end_bpm: 120.0 * (1.0 + percent / 100.0),
            },
            EventStyle::Standard,
            4,
            300_000,
            seed + index as u64,
        ));
        specs.push(spec(
            format!("ramp-minus-{percent:.2}pct"),
            FixtureFamily::TempoDrift,
            TempoProfile::LinearRamp {
                start_bpm: 120.0,
                end_bpm: 120.0 * (1.0 - percent / 100.0),
            },
            EventStyle::Standard,
            4,
            300_000,
            seed + 100 + index as u64,
        ));
    }
    specs.push(spec(
        "tempo-step-return",
        FixtureFamily::TempoDrift,
        TempoProfile::StepReturn {
            base_bpm: 120.0,
            step_bpm: 8.0,
            start_micros: 4_000_000,
            end_micros: 7_000_000,
        },
        EventStyle::Standard,
        4,
        300_000,
        seed,
    ));
    for (id, style) in [
        ("kick-only", EventStyle::KickOnly),
        ("snare-only", EventStyle::SnareOnly),
        ("hats-only", EventStyle::HatsOnly),
        ("attenuated-kick", EventStyle::AttenuatedKick),
        ("kick-removed", EventStyle::KickRemoved),
        ("syncopated-kick", EventStyle::SyncopatedKick),
    ] {
        specs.push(spec(
            id,
            FixtureFamily::Percussion,
            TempoProfile::Constant { bpm: 120.0 },
            style,
            4,
            350_000,
            seed,
        ));
    }
    for (id, style) in [
        ("clear-downbeat", EventStyle::ClearDownbeat),
        ("no-beat-one-kick", EventStyle::NoBeatOneKick),
        ("displaced-accent", EventStyle::DisplacedAccent),
        ("pickup-note", EventStyle::Pickup),
        ("breakdown-reentry", EventStyle::BreakdownReentry),
    ] {
        specs.push(spec(
            id,
            FixtureFamily::DownbeatAmbiguity,
            TempoProfile::Constant { bpm: 128.0 },
            style,
            4,
            600_000,
            seed,
        ));
    }
    for (meter, id) in [
        (2, "meter-2-4"),
        (3, "meter-3-4"),
        (4, "meter-4-4"),
        (6, "meter-6-8"),
    ] {
        specs.push(spec(
            id,
            FixtureFamily::Meter,
            TempoProfile::Constant { bpm: 120.0 },
            EventStyle::ClearDownbeat,
            meter,
            450_000,
            seed,
        ));
    }
    let transform_base = "constant-128.0";
    for (id, transform) in [
        ("transform-gain", TransformKind::Gain { factor: 0.35 }),
        ("transform-compression", TransformKind::Compression),
        ("transform-eq", TransformKind::Eq),
        ("transform-high-pass", TransformKind::HighPass),
        ("transform-low-pass", TransformKind::LowPass),
        ("transform-mono", TransformKind::Mono),
        ("transform-stereo", TransformKind::Stereo),
        (
            "transform-sample-rate",
            TransformKind::SampleRate {
                sample_rate: 16_000,
            },
        ),
    ] {
        let mut fixture = spec(
            id,
            FixtureFamily::Transform,
            TempoProfile::Constant { bpm: 128.0 },
            EventStyle::Standard,
            4,
            0,
            seed,
        );
        fixture.transform = transform;
        if matches!(&fixture.transform, TransformKind::Stereo) {
            fixture.channels = 2;
        }
        if let TransformKind::SampleRate { sample_rate } = &fixture.transform {
            fixture.sample_rate = *sample_rate;
        }
        fixture.base_id = Some(transform_base.to_owned());
        specs.push(fixture);
    }
    let fixtures = specs
        .iter()
        .map(|spec| {
            let generated = generate_fixture(spec)?;
            Ok(FixtureRecord {
                spec: generated.spec,
                truth: generated.truth,
                audio_sha256: generated.audio_sha256,
            })
        })
        .collect::<Result<Vec<_>, LabError>>()?;
    Ok(SyntheticCorpusManifest {
        schema_version: LAB_SCHEMA_VERSION,
        split: "development".into(),
        seed,
        fixtures,
    })
}

fn spec(
    id: impl Into<String>,
    family: FixtureFamily,
    tempo: TempoProfile,
    event_style: EventStyle,
    meter: u8,
    lead_in_micros: u64,
    seed: u64,
) -> FixtureSpec {
    FixtureSpec {
        id: id.into(),
        family,
        duration_micros: DEFAULT_DURATION_MICROS,
        sample_rate: DEFAULT_SAMPLE_RATE,
        channels: 1,
        lead_in_micros: 500_000 + lead_in_micros,
        meter,
        tempo,
        event_style,
        transform: TransformKind::None,
        base_id: None,
        seed,
    }
}

pub fn generate_fixture(spec: &FixtureSpec) -> Result<SyntheticFixture, LabError> {
    spec.validate()?;
    let truth = build_truth(spec);
    let mut audio = synthesize_audio(spec, &truth);
    apply_transform(&mut audio, spec);
    if spec.channels == 2 {
        let mut interleaved = Vec::with_capacity(audio.len() * 2);
        for sample in &audio {
            interleaved.push(*sample);
            interleaved.push(*sample * 0.98);
        }
        audio = interleaved;
    }
    if audio.len() > MAX_AUDIO_SAMPLES {
        return Err(LabError::InvalidInput(
            "fixture exceeds bounded audio memory".into(),
        ));
    }
    let audio_sha256 = hash_pcm(&audio);
    Ok(SyntheticFixture {
        spec: spec.clone(),
        audio,
        truth,
        audio_sha256,
    })
}

fn build_truth(spec: &FixtureSpec) -> AnalysisGroundTruth {
    let mut beats = Vec::new();
    let mut time = spec.lead_in_micros;
    while time < spec.duration_micros {
        beats.push(time);
        let bpm = spec.tempo.bpm_at(time, spec.duration_micros).max(1.0);
        time = time.saturating_add((60_000_000.0 / bpm as f64).round() as u64);
    }
    let downbeats = beats
        .iter()
        .enumerate()
        .filter_map(|(index, _)| index.is_multiple_of(spec.meter as usize).then_some(index))
        .collect();
    let tempo = match spec.tempo {
        TempoProfile::Constant { bpm } => Some(TempoTruth {
            primary_bpm: bpm,
            valid_alternates_bpm: vec![bpm / 2.0, bpm * 2.0],
        }),
        _ => None,
    };
    let tempo_segments = match spec.tempo {
        TempoProfile::Constant { bpm } => vec![TempoSegmentTruth {
            start_micros: spec.lead_in_micros,
            end_micros: spec.duration_micros,
            start_bpm: bpm,
            end_bpm: bpm,
        }],
        TempoProfile::LinearRamp { start_bpm, end_bpm } => vec![TempoSegmentTruth {
            start_micros: spec.lead_in_micros,
            end_micros: spec.duration_micros,
            start_bpm,
            end_bpm,
        }],
        TempoProfile::StepReturn {
            base_bpm,
            step_bpm,
            start_micros,
            end_micros,
        } => vec![
            TempoSegmentTruth {
                start_micros: 0,
                end_micros: start_micros,
                start_bpm: base_bpm,
                end_bpm: base_bpm,
            },
            TempoSegmentTruth {
                start_micros,
                end_micros,
                start_bpm: base_bpm + step_bpm,
                end_bpm: base_bpm + step_bpm,
            },
            TempoSegmentTruth {
                start_micros: end_micros,
                end_micros: spec.duration_micros,
                start_bpm: base_bpm,
                end_bpm: base_bpm,
            },
        ],
    };
    AnalysisGroundTruth {
        tempo,
        meter: Some(spec.meter),
        beat_times_micros: beats,
        downbeats,
        tempo_segments,
        duration_micros: spec.duration_micros,
    }
}

fn synthesize_audio(spec: &FixtureSpec, truth: &AnalysisGroundTruth) -> Vec<f32> {
    let sample_count =
        (spec.duration_micros as f64 * spec.sample_rate as f64 / 1_000_000.0).round() as usize;
    let mut audio = vec![0.0; sample_count];
    let mut rng = Lcg::new(spec.seed);
    for (index, &beat_micros) in truth.beat_times_micros.iter().enumerate() {
        let beat = index % spec.meter as usize;
        let allow_kick = match spec.event_style {
            EventStyle::KickRemoved | EventStyle::SnareOnly | EventStyle::HatsOnly => false,
            EventStyle::StrongEverySecondBeat => beat.is_multiple_of(2),
            EventStyle::NoBeatOneKick | EventStyle::DisplacedAccent => beat != 0,
            EventStyle::Pickup => beat != 0,
            EventStyle::BreakdownReentry => {
                beat != 0 || (beat_micros / 1_000_000).is_multiple_of(2)
            }
            EventStyle::SyncopatedKick => !beat.is_multiple_of(2),
            _ => true,
        };
        let kick_gain = match spec.event_style {
            EventStyle::AttenuatedKick => 0.20,
            EventStyle::KickOnly | EventStyle::ClearDownbeat => 0.90,
            _ => 0.65,
        };
        if allow_kick
            && !matches!(
                spec.event_style,
                EventStyle::SnareOnly | EventStyle::HatsOnly
            )
        {
            add_burst(&mut audio, spec.sample_rate, beat_micros, 55.0, kick_gain);
        }
        if matches!(
            spec.event_style,
            EventStyle::SnareOnly
                | EventStyle::Standard
                | EventStyle::ClearDownbeat
                | EventStyle::DisplacedAccent
                | EventStyle::Pickup
                | EventStyle::BreakdownReentry
        ) && !beat.is_multiple_of(2)
        {
            add_burst(&mut audio, spec.sample_rate, beat_micros, 180.0, 0.55);
        }
        if matches!(
            spec.event_style,
            EventStyle::HatsOnly
                | EventStyle::WeakSubdivision
                | EventStyle::Standard
                | EventStyle::StrongEverySecondBeat
        ) {
            add_burst(
                &mut audio,
                spec.sample_rate,
                beat_micros,
                3_200.0,
                if matches!(spec.event_style, EventStyle::HatsOnly) {
                    0.75
                } else {
                    0.25
                },
            );
        }
        if matches!(
            spec.event_style,
            EventStyle::WeakSubdivision | EventStyle::HatsOnly
        ) {
            let interval = truth
                .beat_times_micros
                .get(index + 1)
                .copied()
                .unwrap_or(beat_micros + 500_000)
                .saturating_sub(beat_micros);
            add_burst(
                &mut audio,
                spec.sample_rate,
                beat_micros.saturating_add(interval / 2),
                3_500.0,
                0.30,
            );
        }
        if matches!(spec.event_style, EventStyle::DisplacedAccent) && beat == 0 {
            add_burst(
                &mut audio,
                spec.sample_rate,
                beat_micros.saturating_add(150_000),
                120.0,
                0.9,
            );
        }
        if matches!(spec.event_style, EventStyle::Pickup) && beat == 0 {
            add_burst(
                &mut audio,
                spec.sample_rate,
                beat_micros.saturating_sub(180_000),
                1_000.0,
                0.6,
            );
        }
    }
    if matches!(spec.family, FixtureFamily::MissingBeat) {
        let missing = if spec.id.contains("two") {
            [4, 5]
        } else {
            [4, usize::MAX]
        };
        for index in missing {
            if let Some(&time) = truth.beat_times_micros.get(index) {
                erase_burst(&mut audio, spec.sample_rate, time);
            }
        }
    }
    if matches!(spec.family, FixtureFamily::ExtraTransient) {
        for &time in truth.beat_times_micros.iter().skip(2).step_by(5) {
            add_burst(&mut audio, spec.sample_rate, time + 230_000, 2_800.0, 0.95);
        }
        if spec.id.contains("fill") {
            for offset in [80_000, 160_000, 240_000] {
                add_burst(
                    &mut audio,
                    spec.sample_rate,
                    6_000_000 + offset,
                    1_800.0,
                    0.65,
                );
            }
        }
    }
    if matches!(spec.family, FixtureFamily::DownbeatAmbiguity)
        && spec.event_style == EventStyle::BreakdownReentry
    {
        for (index, sample) in audio.iter_mut().enumerate() {
            let time = index as u64 * 1_000_000 / spec.sample_rate as u64;
            if (3_000_000..5_000_000).contains(&time) {
                *sample *= 0.05;
            }
        }
        add_burst(&mut audio, spec.sample_rate, 5_000_000, 55.0, 1.0);
    }
    for sample in &mut audio {
        *sample += (rng.next_f32() - 0.5) * 0.0002;
        *sample = sample.clamp(-1.0, 1.0);
    }
    audio
}

fn add_burst(audio: &mut [f32], sample_rate: u32, time_micros: u64, frequency: f32, gain: f32) {
    let center = time_micros as f64 * sample_rate as f64 / 1_000_000.0;
    let radius = (sample_rate as f64 * 0.06) as isize;
    let center_index = center.round() as isize;
    for offset in -radius..=radius {
        let index = center_index + offset;
        if let Some(sample) = audio.get_mut(index as usize) {
            let seconds = offset as f32 / sample_rate as f32;
            let envelope = (-seconds.abs() * 70.0).exp();
            *sample += gain * envelope * (std::f32::consts::TAU * frequency * seconds).sin();
        }
    }
}

fn erase_burst(audio: &mut [f32], sample_rate: u32, time_micros: u64) {
    let center = time_micros as f64 * sample_rate as f64 / 1_000_000.0;
    let radius = (sample_rate as f64 * 0.07) as isize;
    for offset in -radius..=radius {
        if let Some(sample) = audio.get_mut((center.round() as isize + offset) as usize) {
            *sample *= 0.03;
        }
    }
}

fn apply_transform(audio: &mut [f32], spec: &FixtureSpec) {
    match spec.transform {
        TransformKind::None | TransformKind::Mono | TransformKind::Stereo => {}
        TransformKind::Gain { factor } => {
            for sample in audio {
                *sample *= factor;
            }
        }
        TransformKind::Compression => {
            for sample in audio {
                *sample = (*sample * 3.0).tanh() / 3.0;
            }
        }
        TransformKind::Eq => {
            let mut previous = 0.0;
            for sample in audio {
                let current = *sample;
                *sample = current * 0.7 + previous * 0.3;
                previous = current;
            }
        }
        TransformKind::HighPass => {
            let mut previous = 0.0;
            for sample in audio {
                let current = *sample;
                *sample = current - previous * 0.995;
                previous = current;
            }
        }
        TransformKind::LowPass => {
            let mut state = 0.0;
            for sample in audio {
                state = state * 0.85 + *sample * 0.15;
                *sample = state;
            }
        }
        TransformKind::SampleRate { .. } => {}
    }
}

#[derive(Clone, Copy)]
struct Lcg(u64);
impl Lcg {
    fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }
    fn next_f32(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        ((self.0 >> 32) as u32) as f32 / u32::MAX as f32
    }
}

pub fn hash_pcm(audio: &[f32]) -> String {
    let mut digest = Sha256::new();
    for sample in audio {
        digest.update(sample.to_le_bytes());
    }
    format!("{:x}", digest.finalize())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExternalAnalysisObservation {
    pub schema_version: u32,
    pub sample_id: String,
    pub audio_sha256: String,
    pub observer: ObserverIdentity,
    pub analysis_settings: ObservationSettings,
    pub observed: ObservedAnalysis,
    pub timing: ObservationTiming,
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq, Ord, PartialOrd)]
pub struct ObserverIdentity {
    pub product: String,
    pub version: String,
    pub platform: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ObservationSettings {
    pub beat_grid_enabled: Option<bool>,
    pub tempo_range_bpm: Option<(f32, f32)>,
    pub meter_mode: Option<String>,
    pub key_mode: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ObservedAnalysis {
    pub reported_bpm: Option<f32>,
    pub beatgrid_times_micros: Option<Vec<u64>>,
    pub downbeat_indices: Option<Vec<usize>>,
    pub grid_phase_micros: Option<u64>,
    pub musical_key: Option<String>,
    pub meter: Option<u8>,
    pub analysis_complete: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ObservationTiming {
    pub analysis_elapsed_millis: Option<u64>,
    pub observed_duration_micros: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExternalObservationDocument {
    pub schema_version: u32,
    pub observations: Vec<ExternalAnalysisObservation>,
}

impl ExternalObservationDocument {
    pub fn load(path: &Path) -> Result<Self, LabError> {
        let document: Self = serde_json::from_slice(&fs::read(path)?)?;
        document.validate()?;
        Ok(document)
    }

    pub fn validate(&self) -> Result<(), LabError> {
        if self.schema_version != OBSERVATION_SCHEMA_VERSION {
            return Err(LabError::InvalidInput(format!(
                "unsupported external observation schema {}; expected {}",
                self.schema_version, OBSERVATION_SCHEMA_VERSION
            )));
        }
        let mut records = BTreeSet::new();
        for observation in &self.observations {
            if observation.schema_version != OBSERVATION_SCHEMA_VERSION
                || observation.sample_id.trim().is_empty()
                || observation.audio_sha256.len() != 64
                || !observation
                    .audio_sha256
                    .chars()
                    .all(|c| c.is_ascii_hexdigit())
            {
                return Err(LabError::InvalidInput(format!(
                    "invalid external observation record {}",
                    observation.sample_id
                )));
            }
            if observation.observer.product.trim().is_empty()
                || observation.observer.version.trim().is_empty()
            {
                return Err(LabError::InvalidInput(
                    "external observer identity is required".into(),
                ));
            }
            let settings_key = observation_settings_key(&observation.analysis_settings);
            let record_key = format!(
                "{}\u{1f}{}\u{1f}{}\u{1f}{}",
                observation.sample_id,
                observation.observer.product,
                observation.observer.version,
                settings_key
            );
            if !records.insert(record_key) {
                return Err(LabError::InvalidInput(format!(
                    "duplicate external observation for {} {}@{} with identical settings",
                    observation.sample_id,
                    observation.observer.product,
                    observation.observer.version
                )));
            }
            if let Some((minimum, maximum)) = observation.analysis_settings.tempo_range_bpm
                && (!minimum.is_finite()
                    || !maximum.is_finite()
                    || minimum <= 0.0
                    || maximum < minimum)
            {
                return Err(LabError::InvalidInput(
                    "external tempo range must be finite, positive, and ordered".into(),
                ));
            }
            if let Some(bpm) = observation.observed.reported_bpm
                && (!bpm.is_finite() || bpm <= 0.0)
            {
                return Err(LabError::InvalidInput(
                    "external reported BPM must be finite and positive".into(),
                ));
            }
            if observation
                .observed
                .beatgrid_times_micros
                .as_ref()
                .is_some_and(|beats| beats.windows(2).any(|window| window[0] >= window[1]))
            {
                return Err(LabError::InvalidInput(
                    "external beatgrid must be strictly increasing".into(),
                ));
            }
            if let Some(meter) = observation.observed.meter
                && !matches!(meter, 2 | 3 | 4 | 6)
            {
                return Err(LabError::InvalidInput(
                    "external meter must be one of 2, 3, 4, or 6".into(),
                ));
            }
            if let Some(indices) = observation.observed.downbeat_indices.as_ref() {
                if let Some(beats) = observation.observed.beatgrid_times_micros.as_ref() {
                    if indices.iter().any(|index| *index >= beats.len()) {
                        return Err(LabError::InvalidInput(
                            "external downbeat index is outside the supplied beatgrid".into(),
                        ));
                    }
                } else if !indices.is_empty() {
                    return Err(LabError::InvalidInput(
                        "external downbeats require a supplied beatgrid".into(),
                    ));
                }
            }
            if let Some(duration) = observation.timing.observed_duration_micros {
                if observation
                    .observed
                    .beatgrid_times_micros
                    .as_ref()
                    .is_some_and(|beats| beats.iter().any(|beat| *beat > duration))
                {
                    return Err(LabError::InvalidInput(
                        "external beatgrid exceeds the observed duration".into(),
                    ));
                }
                if observation
                    .observed
                    .grid_phase_micros
                    .is_some_and(|phase| phase > duration)
                {
                    return Err(LabError::InvalidInput(
                        "external grid phase exceeds the observed duration".into(),
                    ));
                }
            }
        }
        Ok(())
    }
}

fn observation_settings_key(settings: &ObservationSettings) -> String {
    format!(
        "beats={:?};tempo={:?};meter={:?};key={:?}",
        settings.beat_grid_enabled,
        settings.tempo_range_bpm,
        settings.meter_mode,
        settings.key_mode
    )
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NormalizedAnalysis {
    pub analyzer: String,
    pub duration_micros: u64,
    pub beats: Vec<NormalizedBeat>,
    pub tempo_hypotheses: Vec<NormalizedTempoHypothesis>,
    pub meter_hypotheses: Vec<NormalizedMeterHypothesis>,
    pub resolved_meter: Option<u8>,
    pub resolved_meter_phase: Option<u8>,
    pub structure: NormalizedStructure,
    pub confidence: ConfidenceEvidence,
    pub provenance: BTreeMap<String, ProvenanceView>,
}

/// Availability-preserving representation of an external observation.
///
/// This intentionally is not `NormalizedAnalysis`: an external observer may
/// omit one field while reporting another, and `Some(vec![])` is a meaningful
/// completed zero-result observation rather than absence.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NormalizedExternalAnalysis {
    pub analyzer: String,
    pub duration_micros: u64,
    pub beats: Option<Vec<NormalizedBeat>>,
    pub reported_bpm: Option<f32>,
    pub downbeat_indices: Option<Vec<usize>>,
    pub grid_phase_micros: Option<u64>,
    pub meter: Option<u8>,
    pub analysis_complete: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NormalizedBeat {
    pub time_micros: u64,
    pub timing_confidence: f32,
    pub beat_model_score: Option<f32>,
    pub onset_support: Option<f32>,
    pub low_frequency_support: Option<f32>,
    pub downbeat_evidence: Option<f32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NormalizedTempoHypothesis {
    pub bpm: f32,
    pub relative_weight: f32,
    pub relation: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NormalizedMeterHypothesis {
    pub beats_per_bar: u8,
    pub downbeat_phase: u8,
    pub score: f32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct NormalizedStructure {
    pub phrase_boundaries: Vec<NormalizedPhraseBoundary>,
    pub sections: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NormalizedPhraseBoundary {
    pub beat_index: usize,
    pub strength: f32,
    pub source: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ConfidenceEvidence {
    pub timing: Vec<f32>,
    pub beat_model: Vec<f32>,
    pub onset: Vec<f32>,
    pub low_frequency: Vec<f32>,
    pub downbeat: Vec<f32>,
    pub structure: Vec<f32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProvenanceView {
    pub method: String,
    pub model: Option<String>,
    pub confidence: Option<f32>,
}

pub fn normalize_v2(analysis: &TrackAnalysisV2) -> NormalizedAnalysis {
    let beats = analysis
        .rhythm
        .beats
        .iter()
        .map(|beat| NormalizedBeat {
            time_micros: duration_micros(beat.time),
            timing_confidence: beat.timing_confidence.get(),
            beat_model_score: beat.beat_model_score.map(|v| v.get()),
            onset_support: beat.onset_support.map(|v| v.get()),
            low_frequency_support: beat.low_frequency_support.map(|v| v.get()),
            downbeat_evidence: beat.downbeat_model_score.map(|v| v.get()),
        })
        .collect::<Vec<_>>();
    let mut confidence = ConfidenceEvidence::default();
    for beat in &beats {
        confidence.timing.push(beat.timing_confidence);
        if let Some(v) = beat.beat_model_score {
            confidence.beat_model.push(v);
        }
        if let Some(v) = beat.onset_support {
            confidence.onset.push(v);
        }
        if let Some(v) = beat.low_frequency_support {
            confidence.low_frequency.push(v);
        }
        if let Some(v) = beat.downbeat_evidence {
            confidence.downbeat.push(v);
        }
    }
    confidence.structure.extend(
        analysis
            .structure
            .phrase_boundaries
            .iter()
            .map(|boundary| boundary.strength.get()),
    );
    let provenance = [
        ("rhythm", analysis.provenance.rhythm.as_ref()),
        ("structure", analysis.provenance.structure.as_ref()),
        ("tonal", analysis.provenance.tonal.as_ref()),
        ("vocal", analysis.provenance.vocal.as_ref()),
        ("energy", analysis.provenance.energy.as_ref()),
        ("cue", analysis.provenance.cue.as_ref()),
    ]
    .into_iter()
    .filter_map(|(name, value)| {
        value.map(|value| {
            (
                name.to_owned(),
                ProvenanceView {
                    method: format_method(&value.method),
                    model: value
                        .model
                        .as_ref()
                        .map(|model| format!("{}@{}", model.id, model.version)),
                    confidence: value.confidence.map(|value| value.get()),
                },
            )
        })
    })
    .collect();
    NormalizedAnalysis {
        analyzer: analysis.provenance.analyzer.clone(),
        duration_micros: duration_micros(analysis.duration),
        beats,
        tempo_hypotheses: analysis
            .rhythm
            .tempo_hypotheses
            .iter()
            .map(|hypothesis| NormalizedTempoHypothesis {
                bpm: hypothesis.bpm,
                relative_weight: hypothesis.relative_weight.get(),
                relation: format_relation(hypothesis.relation),
            })
            .collect(),
        meter_hypotheses: analysis
            .rhythm
            .meter_hypotheses
            .iter()
            .map(|hypothesis| NormalizedMeterHypothesis {
                beats_per_bar: hypothesis.beats_per_bar,
                downbeat_phase: hypothesis.downbeat_phase,
                score: hypothesis.score.get(),
            })
            .collect(),
        resolved_meter: analysis.rhythm.resolved_meter(),
        resolved_meter_phase: analysis
            .rhythm
            .resolved_meter_hypothesis()
            .map(|hypothesis| hypothesis.downbeat_phase),
        structure: NormalizedStructure {
            phrase_boundaries: analysis
                .structure
                .phrase_boundaries
                .iter()
                .map(|boundary| NormalizedPhraseBoundary {
                    beat_index: boundary.beat_index,
                    strength: boundary.strength.get(),
                    source: format_source(boundary.source),
                })
                .collect(),
            sections: analysis.structure.sections.len(),
        },
        confidence,
        provenance,
    }
}

fn format_method(method: &AnalysisMethod) -> String {
    format!("{method:?}").to_ascii_lowercase()
}
fn format_relation(relation: TempoRelation) -> String {
    format!("{relation:?}").to_ascii_lowercase()
}
fn format_source(source: PhraseBoundarySource) -> String {
    format!("{source:?}").to_ascii_lowercase()
}
fn duration_micros(duration: Duration) -> u64 {
    duration.as_micros().min(u128::from(u64::MAX)) as u64
}
fn duration_from_micros(micros: u64) -> Duration {
    Duration::from_micros(micros)
}

pub fn analyze_fixture(
    fixture: &SyntheticFixture,
    mode: AnalyzerMode,
) -> Result<NormalizedAnalysis, LabError> {
    let mono = downmix(&fixture.audio, fixture.spec.channels);
    let analysis_audio = if fixture.spec.sample_rate == DEFAULT_SAMPLE_RATE {
        mono
    } else {
        resample(&mono, fixture.spec.sample_rate, DEFAULT_SAMPLE_RATE)
    };
    let legacy =
        wotoha_core::audio_analysis::analyze_mono_pcm(&analysis_audio, DEFAULT_SAMPLE_RATE)
            .ok_or_else(|| {
                LabError::InvalidInput(format!(
                    "Wotoha classical analysis rejected {}",
                    fixture.spec.id
                ))
            })?;
    let v2 = if mode == AnalyzerMode::Hybrid {
        let low_band = low_band_1khz(&analysis_audio);
        if let Some(rhythm) =
            wotoha_runtime::analyze_neural_rhythm(&analysis_audio, DEFAULT_SAMPLE_RATE, &low_band)
        {
            wotoha_runtime::track_analysis_v2_from_legacy_rhythm(&legacy, rhythm, true)
        } else {
            wotoha_runtime::track_analysis_v2_from_legacy(&legacy)
        }
    } else {
        wotoha_runtime::track_analysis_v2_from_legacy(&legacy)
    }
    .ok_or_else(|| {
        LabError::InvalidInput(format!("Wotoha V2 adaptation rejected {}", fixture.spec.id))
    })?;
    Ok(normalize_v2(&v2))
}

fn downmix(samples: &[f32], channels: u8) -> Vec<f32> {
    let channels = usize::from(channels.max(1));
    if channels == 1 {
        return samples.to_vec();
    }
    samples
        .chunks(channels)
        .map(|frame| frame.iter().copied().sum::<f32>() / frame.len().max(1) as f32)
        .collect()
}

fn low_band_1khz(samples: &[f32]) -> Vec<f32> {
    let mut filter = LowBandFilter::new(DEFAULT_SAMPLE_RATE).expect("fixed sample rate is valid");
    let filtered = samples
        .iter()
        .map(|sample| filter.process(*sample))
        .collect::<Vec<_>>();
    filtered
        .chunks(22)
        .map(|chunk| chunk.iter().sum::<f32>() / chunk.len() as f32)
        .collect()
}

fn resample(samples: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to {
        return samples.to_vec();
    }
    let length = ((samples.len() as f64 * to as f64 / from as f64).round() as usize).max(1);
    (0..length)
        .map(|index| {
            let position = index as f64 * from as f64 / to as f64;
            let left = position.floor() as usize;
            let right = (left + 1).min(samples.len().saturating_sub(1));
            let fraction = position.fract() as f32;
            samples.get(left).copied().unwrap_or_default() * (1.0 - fraction)
                + samples.get(right).copied().unwrap_or_default() * fraction
        })
        .collect()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EvaluationOptions {
    pub mode: AnalyzerMode,
    pub split: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EvaluationReport {
    pub schema_version: u32,
    pub evaluator: String,
    pub split: String,
    pub overall: GroupMetrics,
    pub by_fixture_family: BTreeMap<String, GroupMetrics>,
    pub by_tempo_range: BTreeMap<String, GroupMetrics>,
    pub by_meter: BTreeMap<String, GroupMetrics>,
    pub by_transform: BTreeMap<String, TransformMetrics>,
    pub half_double_errors: BTreeMap<String, usize>,
    pub downbeat_errors: BTreeMap<String, usize>,
    pub variable_tempo: VariableTempoMetrics,
    pub confidence_calibration: Vec<CalibrationBin>,
    pub failure_clusters: BTreeMap<String, usize>,
    pub external: ExternalReport,
    pub per_track: Vec<TrackEvaluation>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GroupMetrics {
    pub tracks: usize,
    pub beat: BeatMetrics,
    pub tempo: TempoMetrics,
    pub grid_phase: PhaseMetrics,
    pub downbeat: DownbeatMetrics,
    pub meter: MeterMetrics,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BeatMetrics {
    pub predicted: usize,
    pub truth: usize,
    pub matched: usize,
    pub mae_ms: Option<f64>,
    pub p50_ms: Option<f64>,
    pub p95_ms: Option<f64>,
    pub precision_at_tolerance: BTreeMap<String, f64>,
    pub recall_at_tolerance: BTreeMap<String, f64>,
    #[serde(skip)]
    matched_errors_ms: Vec<f64>,
    #[serde(skip)]
    matched_by_tolerance: BTreeMap<String, usize>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TempoMetrics {
    pub truth_bpm: Option<f32>,
    pub primary_bpm: Option<f32>,
    pub absolute_error_bpm: Option<f32>,
    pub relative_error: Option<f32>,
    pub primary_correct: Option<bool>,
    pub scored_tracks: usize,
    pub primary_correct_count: usize,
    pub primary_correct_rate: Option<f64>,
    pub correct_hypothesis_top_n: BTreeMap<String, bool>,
    pub top_n_scored_tracks: BTreeMap<String, usize>,
    pub top_n_correct_count: BTreeMap<String, usize>,
    pub correct_hypothesis_top_n_rate: BTreeMap<String, f64>,
    pub canonical_hypothesis_present: Option<bool>,
    pub musically_valid_hypothesis_present: Option<bool>,
    pub relation_counts: BTreeMap<String, usize>,
    pub relation_error: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PhaseMetrics {
    pub error_ms: Option<f64>,
    pub period_ms: Option<f64>,
    pub correct: Option<bool>,
    pub scored_tracks: usize,
    pub correct_count: usize,
    pub correct_rate: Option<f64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DownbeatMetrics {
    pub truth_count: usize,
    pub predicted_count: usize,
    pub first_offset_beats: Option<i32>,
    pub phase_correct: Option<bool>,
    pub scored_tracks: usize,
    pub phase_correct_count: usize,
    pub phase_correct_rate: Option<f64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MeterMetrics {
    pub truth: Option<u8>,
    pub predicted: Option<u8>,
    pub status: String,
    pub scored_tracks: usize,
    pub correct_count: usize,
    pub unknown_count: usize,
    pub wrong_count: usize,
    pub correct_rate: Option<f64>,
    pub unknown_rate: Option<f64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct VariableTempoMetrics {
    pub tracks: usize,
    pub local_tempo_mae_bpm: Option<f64>,
    pub phase_drift_p95_ms: Option<f64>,
    pub change_tracking_delay_ms: Option<f64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CalibrationBin {
    pub lower: f32,
    pub upper: f32,
    pub observations: usize,
    pub matched_count: usize,
    pub unmatched_predicted_count: usize,
    pub match_rate: Option<f64>,
    pub mean_confidence: Option<f64>,
    pub mean_absolute_error_ms: Option<f64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TransformMetrics {
    pub transform: String,
    pub tracks: usize,
    pub matched_beats: usize,
    pub missing_beats: usize,
    pub extra_beats: usize,
    pub mean_beat_displacement_ms: Option<f64>,
    pub p95_beat_displacement_ms: Option<f64>,
    pub tempo_interpretation_changes: usize,
    pub downbeat_changes: usize,
    pub meter_changes: usize,
    pub confidence_change: Option<f64>,
    #[serde(skip)]
    displacement_ms: Vec<f64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ExternalReport {
    pub observations: usize,
    pub complete_observations: usize,
    pub incomplete_observations: usize,
    pub by_observer: BTreeMap<String, ExternalObserverReport>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ExternalObserverReport {
    pub product: String,
    pub version: String,
    pub settings: String,
    pub observations: usize,
    pub complete_observations: usize,
    pub incomplete_observations: usize,
    pub beat_observations: usize,
    pub tempo_observations: usize,
    pub meter_observations: usize,
    pub downbeat_observations: usize,
    pub grid_phase_observations: usize,
    pub wotoha_vs_truth: GroupMetrics,
    pub external_vs_truth: GroupMetrics,
    pub wotoha_vs_external: GroupMetrics,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrackEvaluation {
    pub sample_id: String,
    pub family: String,
    pub audio_sha256: String,
    pub truth: AnalysisGroundTruth,
    pub wotoha: NormalizedAnalysis,
    pub metrics: GroupMetrics,
    pub failure_clusters: Vec<String>,
    pub human_review: Option<HumanReviewLabel>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HumanReviewLabel {
    WotohaCorrect,
    ExternalCorrect,
    BothAcceptable,
    Ambiguous,
    NeitherCorrect,
}

pub fn evaluate_manifest(
    manifest: &SyntheticCorpusManifest,
    external: Option<&ExternalObservationDocument>,
    options: EvaluationOptions,
) -> Result<EvaluationReport, LabError> {
    evaluate_manifest_with_cancel(manifest, external, options, &AtomicBool::new(false))
}

pub fn evaluate_manifest_with_cancel(
    manifest: &SyntheticCorpusManifest,
    external: Option<&ExternalObservationDocument>,
    options: EvaluationOptions,
    cancelled: &AtomicBool,
) -> Result<EvaluationReport, LabError> {
    manifest.validate()?;
    if let Some(external) = external {
        external.validate()?;
    }
    let mut tracks = Vec::with_capacity(manifest.fixtures.len());
    let mut normalized_by_id = BTreeMap::new();
    for record in &manifest.fixtures {
        if cancelled.load(Ordering::Relaxed) {
            return Err(LabError::Cancelled);
        }
        let fixture = generate_fixture(&record.spec)?;
        if fixture.audio_sha256 != record.audio_sha256 {
            return Err(LabError::HashMismatch {
                sample_id: record.spec.id.clone(),
                expected: record.audio_sha256.clone(),
                actual: fixture.audio_sha256,
            });
        }
        let wotoha = analyze_fixture(&fixture, options.mode)?;
        normalized_by_id.insert(record.spec.id.clone(), wotoha.clone());
        let metrics = metrics_for(&record.truth, &wotoha);
        let failures = failure_clusters(&record.truth, &wotoha, &metrics);
        tracks.push(TrackEvaluation {
            sample_id: record.spec.id.clone(),
            family: record.spec.family.as_str().into(),
            audio_sha256: record.audio_sha256.clone(),
            truth: record.truth.clone(),
            wotoha,
            metrics,
            failure_clusters: failures,
            human_review: None,
        });
    }
    let overall = aggregate_groups(tracks.iter().map(|track| &track.metrics));
    let by_fixture_family = group_tracks(tracks.iter(), |track| track.family.clone());
    let by_tempo_range = group_tracks(tracks.iter(), |track| {
        tempo_range(track.truth.tempo.as_ref().map(|tempo| tempo.primary_bpm))
    });
    let by_meter = group_tracks(tracks.iter(), |track| {
        track
            .truth
            .meter
            .map(|meter| meter.to_string())
            .unwrap_or_else(|| "unknown".into())
    });
    let by_transform = transform_metrics(&manifest.fixtures, &normalized_by_id);
    let (half_double_errors, downbeat_errors, failure_clusters) = summarize_failures(&tracks);
    let confidence_calibration = calibration(&tracks);
    let variable_tempo = variable_tempo_metrics(&tracks);
    let external_report = external
        .map(|document| compare_external(document, manifest, &tracks))
        .transpose()?
        .unwrap_or_default();
    Ok(EvaluationReport {
        schema_version: REPORT_SCHEMA_VERSION,
        evaluator: format!("wotoha-analysis-lab/{}", env!("CARGO_PKG_VERSION")),
        split: options.split,
        overall,
        by_fixture_family,
        by_tempo_range,
        by_meter,
        by_transform,
        half_double_errors,
        downbeat_errors,
        variable_tempo,
        confidence_calibration,
        failure_clusters,
        external: external_report,
        per_track: tracks,
    })
}

impl EvaluationReport {
    pub fn human_summary(&self) -> String {
        format!(
            "analysis baseline: {} tracks; beat p50={} ms p95={} ms; beat precision@40ms={:.3}; tempo primary correctness={:.3}; variable-tempo tracks={}; external observations={}",
            self.overall.tracks,
            format_opt(self.overall.beat.p50_ms),
            format_opt(self.overall.beat.p95_ms),
            self.overall
                .beat
                .precision_at_tolerance
                .get("40ms")
                .copied()
                .unwrap_or(0.0),
            self.overall.tempo.primary_correct_rate.unwrap_or(0.0),
            self.variable_tempo.tracks,
            self.external.observations
        )
    }
}

fn metrics_for(truth: &AnalysisGroundTruth, prediction: &NormalizedAnalysis) -> GroupMetrics {
    GroupMetrics {
        tracks: 1,
        beat: beat_metrics(
            &truth.beat_times_micros,
            &prediction
                .beats
                .iter()
                .map(|beat| beat.time_micros)
                .collect::<Vec<_>>(),
        ),
        tempo: tempo_metrics(truth.tempo.as_ref(), &prediction.tempo_hypotheses),
        grid_phase: phase_metrics(truth, prediction),
        downbeat: downbeat_metrics(truth, prediction),
        meter: meter_metrics(truth, prediction),
    }
}

fn match_errors(
    truth: &[u64],
    predicted: &[u64],
    tolerance: Duration,
) -> Vec<(usize, usize, Duration)> {
    let tolerance = duration_micros(tolerance);
    #[derive(Clone, Copy, Default)]
    struct Score {
        matches: usize,
        error: u64,
        choice: u8,
    }

    fn better(candidate: Score, current: Score, priority: u8) -> bool {
        candidate.matches > current.matches
            || (candidate.matches == current.matches
                && (candidate.error < current.error
                    || (candidate.error == current.error && priority < current.choice)))
    }

    let columns = predicted.len() + 1;
    let mut table = vec![Score::default(); (truth.len() + 1) * columns];
    let index =
        |truth_index: usize, predicted_index: usize| truth_index * columns + predicted_index;
    for truth_index in (0..truth.len()).rev() {
        for predicted_index in (0..predicted.len()).rev() {
            let mut best = table[index(truth_index + 1, predicted_index)];
            best.choice = 1;
            let skip_predicted = table[index(truth_index, predicted_index + 1)];
            if better(skip_predicted, best, 2) {
                best = skip_predicted;
                best.choice = 2;
            }
            let error = truth[truth_index].abs_diff(predicted[predicted_index]);
            if error <= tolerance {
                let following = table[index(truth_index + 1, predicted_index + 1)];
                let matched = Score {
                    matches: following.matches + 1,
                    error: following.error.saturating_add(error),
                    choice: 0,
                };
                if better(matched, best, 0) {
                    best = matched;
                }
            }
            table[index(truth_index, predicted_index)] = best;
        }
    }
    let mut pairs = Vec::new();
    let (mut truth_index, mut predicted_index) = (0, 0);
    while truth_index < truth.len() && predicted_index < predicted.len() {
        let score = table[index(truth_index, predicted_index)];
        match score.choice {
            0 => {
                let error = truth[truth_index].abs_diff(predicted[predicted_index]);
                if error <= tolerance {
                    pairs.push((truth_index, predicted_index, duration_from_micros(error)));
                    truth_index += 1;
                    predicted_index += 1;
                } else {
                    break;
                }
            }
            1 => truth_index += 1,
            2 => predicted_index += 1,
            _ => break,
        }
    }
    pairs
}

fn beat_metrics(truth: &[u64], predicted: &[u64]) -> BeatMetrics {
    let pairs = match_errors(truth, predicted, BEAT_MATCH_WINDOW);
    let errors = pairs
        .iter()
        .map(|(_, _, error)| error.as_secs_f64() * 1_000.0)
        .collect::<Vec<_>>();
    let mut precision = BTreeMap::new();
    let mut recall = BTreeMap::new();
    let mut matched_by_tolerance = BTreeMap::new();
    for tolerance in [10_u64, 20, 40, 70] {
        let count = match_errors(truth, predicted, Duration::from_millis(tolerance)).len() as f64;
        matched_by_tolerance.insert(format!("{tolerance}ms"), count as usize);
        precision.insert(
            format!("{tolerance}ms"),
            safe_ratio(count, predicted.len() as f64),
        );
        recall.insert(
            format!("{tolerance}ms"),
            safe_ratio(count, truth.len() as f64),
        );
    }
    BeatMetrics {
        predicted: predicted.len(),
        truth: truth.len(),
        matched: pairs.len(),
        mae_ms: mean(&errors),
        p50_ms: percentile(&errors, 0.50),
        p95_ms: percentile(&errors, 0.95),
        precision_at_tolerance: precision,
        recall_at_tolerance: recall,
        matched_errors_ms: errors,
        matched_by_tolerance,
    }
}

fn tempo_metrics(
    truth: Option<&TempoTruth>,
    hypotheses: &[NormalizedTempoHypothesis],
) -> TempoMetrics {
    let Some(truth) = truth else {
        return TempoMetrics::default();
    };
    let primary = hypotheses
        .iter()
        .find(|hypothesis| hypothesis.relation == "primary")
        .or_else(|| hypotheses.first())
        .map(|hypothesis| hypothesis.bpm);
    let absolute = primary.map(|bpm| (bpm - truth.primary_bpm).abs());
    let relative = absolute.map(|error| error / truth.primary_bpm.max(f32::EPSILON));
    let canonical_present = hypotheses
        .iter()
        .any(|hypothesis| (hypothesis.bpm - truth.primary_bpm).abs() / truth.primary_bpm < 0.005);
    let musically_valid_present = hypotheses.iter().any(|hypothesis| {
        (hypothesis.bpm - truth.primary_bpm).abs() / truth.primary_bpm < 0.005
            || truth.valid_alternates_bpm.iter().any(|alternate| {
                (hypothesis.bpm - *alternate).abs() / alternate.max(f32::EPSILON) < 0.005
            })
    });
    let mut top_n = BTreeMap::new();
    let mut top_n_scored_tracks = BTreeMap::new();
    let mut top_n_correct_count = BTreeMap::new();
    for n in [1, 3, 5] {
        let correct = hypotheses.iter().take(n).any(|hypothesis| {
            (hypothesis.bpm - truth.primary_bpm).abs() / truth.primary_bpm < 0.005
        });
        top_n.insert(n.to_string(), correct);
        top_n_scored_tracks.insert(n.to_string(), 1);
        top_n_correct_count.insert(n.to_string(), usize::from(correct));
    }
    let top_n_rate = top_n
        .iter()
        .map(|(n, correct)| (n.clone(), bool_fraction(*correct)))
        .collect();
    let relation_error = Some(
        primary
            .map(|bpm| relation_label(bpm, truth.primary_bpm))
            .unwrap_or_else(|| "absent".into()),
    );
    let primary_correct = Some(
        primary.is_some_and(|bpm| (bpm - truth.primary_bpm).abs() / truth.primary_bpm < 0.005),
    );
    let mut relation_counts = BTreeMap::new();
    if let Some(relation) = relation_error.as_ref() {
        relation_counts.insert(relation.clone(), 1);
    }
    TempoMetrics {
        truth_bpm: Some(truth.primary_bpm),
        primary_bpm: primary,
        absolute_error_bpm: absolute,
        relative_error: relative,
        primary_correct,
        scored_tracks: 1,
        primary_correct_count: usize::from(primary_correct == Some(true)),
        primary_correct_rate: primary_correct.map(bool_fraction),
        correct_hypothesis_top_n: top_n,
        top_n_scored_tracks,
        top_n_correct_count,
        correct_hypothesis_top_n_rate: top_n_rate,
        canonical_hypothesis_present: Some(canonical_present),
        musically_valid_hypothesis_present: Some(musically_valid_present),
        relation_counts,
        relation_error,
    }
}

fn phase_metrics(truth: &AnalysisGroundTruth, prediction: &NormalizedAnalysis) -> PhaseMetrics {
    if truth.tempo.is_none() {
        return PhaseMetrics::default();
    }
    phase_metrics_at(truth, prediction.beats.first().map(|beat| beat.time_micros))
}

fn phase_metrics_at(truth: &AnalysisGroundTruth, predicted_phase: Option<u64>) -> PhaseMetrics {
    let Some(&truth_first) = truth.beat_times_micros.first() else {
        return PhaseMetrics::default();
    };
    let Some(predicted_first) = predicted_phase else {
        return PhaseMetrics::default();
    };
    let period = truth
        .beat_times_micros
        .windows(2)
        .next()
        .map(|window| window[1].saturating_sub(window[0]))
        .filter(|period| *period > 0);
    let Some(period) = period else {
        return PhaseMetrics::default();
    };
    let raw = predicted_first.abs_diff(truth_first) % period;
    let error = raw.min(period - raw);
    PhaseMetrics {
        error_ms: Some(error as f64 / 1_000.0),
        period_ms: Some(period as f64 / 1_000.0),
        correct: Some(error <= 20_000),
        scored_tracks: 1,
        correct_count: usize::from(error <= 20_000),
        correct_rate: Some(bool_fraction(error <= 20_000)),
    }
}

fn phase_metrics_between(predicted_phase: u64, reference_phase: u64, period: u64) -> PhaseMetrics {
    if period == 0 {
        return PhaseMetrics::default();
    }
    let raw = predicted_phase.abs_diff(reference_phase) % period;
    let error = raw.min(period - raw);
    PhaseMetrics {
        error_ms: Some(error as f64 / 1_000.0),
        period_ms: Some(period as f64 / 1_000.0),
        correct: Some(error <= 20_000),
        scored_tracks: 1,
        correct_count: usize::from(error <= 20_000),
        correct_rate: Some(bool_fraction(error <= 20_000)),
    }
}

fn downbeat_metrics(
    truth: &AnalysisGroundTruth,
    prediction: &NormalizedAnalysis,
) -> DownbeatMetrics {
    let predicted = prediction
        .beats
        .iter()
        .enumerate()
        .filter_map(|(index, beat)| {
            beat.downbeat_evidence
                .filter(|score| *score >= 0.5)
                .map(|_| index)
        })
        .collect::<Vec<_>>();
    if prediction.resolved_meter.is_none() {
        return DownbeatMetrics {
            truth_count: truth.downbeats.len(),
            predicted_count: predicted.len(),
            ..DownbeatMetrics::default()
        };
    }
    downbeat_metrics_from_indices(truth, &predicted)
}

fn downbeat_metrics_from_indices(
    truth: &AnalysisGroundTruth,
    predicted: &[usize],
) -> DownbeatMetrics {
    let offset = truth
        .downbeats
        .first()
        .zip(predicted.first())
        .map(|(truth, predicted)| *predicted as i32 - *truth as i32);
    let phase_correct = truth
        .downbeats
        .first()
        .zip(truth.meter)
        .map(|(_, meter)| offset.is_some_and(|offset| offset.rem_euclid(meter as i32) == 0));
    DownbeatMetrics {
        truth_count: truth.downbeats.len(),
        predicted_count: predicted.len(),
        first_offset_beats: offset,
        phase_correct,
        scored_tracks: usize::from(phase_correct.is_some()),
        phase_correct_count: usize::from(phase_correct == Some(true)),
        phase_correct_rate: phase_correct.map(bool_fraction),
    }
}

fn meter_metrics(truth: &AnalysisGroundTruth, prediction: &NormalizedAnalysis) -> MeterMetrics {
    meter_metrics_value(truth.meter, prediction.resolved_meter)
}

fn meter_metrics_value(truth: Option<u8>, predicted: Option<u8>) -> MeterMetrics {
    let status = match (truth, predicted) {
        (Some(truth), Some(predicted)) if truth == predicted => "correct",
        (Some(_), Some(_)) => "wrong",
        (Some(_), None) => "unknown",
        (None, _) => "not_scored",
    }
    .into();
    MeterMetrics {
        truth,
        predicted,
        correct_count: usize::from(status == "correct"),
        unknown_count: usize::from(status == "unknown"),
        wrong_count: usize::from(status == "wrong"),
        correct_rate: (status == "correct").then_some(1.0),
        unknown_rate: (status == "unknown").then_some(1.0),
        scored_tracks: usize::from(truth.is_some()),
        status,
    }
}

fn aggregate_groups<'a>(groups: impl Iterator<Item = &'a GroupMetrics>) -> GroupMetrics {
    let groups = groups.collect::<Vec<_>>();
    let mut aggregate = GroupMetrics {
        tracks: groups.iter().map(|group| group.tracks).sum(),
        ..GroupMetrics::default()
    };
    aggregate.beat.predicted = groups.iter().map(|group| group.beat.predicted).sum();
    aggregate.beat.truth = groups.iter().map(|group| group.beat.truth).sum();
    aggregate.beat.matched = groups.iter().map(|group| group.beat.matched).sum();
    aggregate.beat.matched_errors_ms = groups
        .iter()
        .flat_map(|group| group.beat.matched_errors_ms.iter().copied())
        .collect();
    aggregate.beat.mae_ms = mean(&aggregate.beat.matched_errors_ms);
    aggregate.beat.p50_ms = percentile(&aggregate.beat.matched_errors_ms, 0.50);
    aggregate.beat.p95_ms = percentile(&aggregate.beat.matched_errors_ms, 0.95);
    for tolerance in ["10ms", "20ms", "40ms", "70ms"] {
        let matched = groups
            .iter()
            .map(|group| {
                group
                    .beat
                    .matched_by_tolerance
                    .get(tolerance)
                    .copied()
                    .unwrap_or_default()
            })
            .sum::<usize>();
        aggregate
            .beat
            .matched_by_tolerance
            .insert(tolerance.into(), matched);
        aggregate.beat.precision_at_tolerance.insert(
            tolerance.into(),
            safe_ratio(matched as f64, aggregate.beat.predicted as f64),
        );
        aggregate.beat.recall_at_tolerance.insert(
            tolerance.into(),
            safe_ratio(matched as f64, aggregate.beat.truth as f64),
        );
    }
    aggregate.tempo.scored_tracks = groups.iter().map(|group| group.tempo.scored_tracks).sum();
    aggregate.tempo.primary_correct_count = groups
        .iter()
        .map(|group| group.tempo.primary_correct_count)
        .sum();
    aggregate.tempo.primary_correct_rate = (aggregate.tempo.scored_tracks > 0).then(|| {
        aggregate.tempo.primary_correct_count as f64 / aggregate.tempo.scored_tracks as f64
    });
    aggregate.tempo.relation_counts = groups
        .iter()
        .flat_map(|group| group.tempo.relation_counts.iter())
        .fold(BTreeMap::new(), |mut counts, (relation, count)| {
            *counts.entry(relation.clone()).or_default() += count;
            counts
        });
    for n in [1, 3, 5] {
        let key = n.to_string();
        let scored = groups
            .iter()
            .map(|group| {
                group
                    .tempo
                    .top_n_scored_tracks
                    .get(&key)
                    .copied()
                    .unwrap_or_default()
            })
            .sum::<usize>();
        let correct = groups
            .iter()
            .map(|group| {
                group
                    .tempo
                    .top_n_correct_count
                    .get(&key)
                    .copied()
                    .unwrap_or_default()
            })
            .sum::<usize>();
        aggregate
            .tempo
            .top_n_scored_tracks
            .insert(key.clone(), scored);
        aggregate
            .tempo
            .top_n_correct_count
            .insert(key.clone(), correct);
        aggregate
            .tempo
            .correct_hypothesis_top_n
            .insert(key.clone(), scored > 0 && correct == scored);
        aggregate
            .tempo
            .correct_hypothesis_top_n_rate
            .insert(key, safe_ratio(correct as f64, scored as f64));
    }
    aggregate.grid_phase.error_ms = weighted_metric(
        groups
            .iter()
            .map(|group| (group.grid_phase.error_ms, group.grid_phase.scored_tracks)),
    );
    aggregate.grid_phase.scored_tracks = groups
        .iter()
        .map(|group| group.grid_phase.scored_tracks)
        .sum();
    aggregate.grid_phase.correct_count = groups
        .iter()
        .map(|group| group.grid_phase.correct_count)
        .sum();
    aggregate.grid_phase.correct_rate = (aggregate.grid_phase.scored_tracks > 0).then(|| {
        aggregate.grid_phase.correct_count as f64 / aggregate.grid_phase.scored_tracks as f64
    });
    aggregate.downbeat.truth_count = groups.iter().map(|group| group.downbeat.truth_count).sum();
    aggregate.downbeat.predicted_count = groups
        .iter()
        .map(|group| group.downbeat.predicted_count)
        .sum();
    aggregate.downbeat.scored_tracks = groups
        .iter()
        .map(|group| group.downbeat.scored_tracks)
        .sum();
    aggregate.downbeat.phase_correct_count = groups
        .iter()
        .map(|group| group.downbeat.phase_correct_count)
        .sum();
    aggregate.downbeat.phase_correct_rate = (aggregate.downbeat.scored_tracks > 0).then(|| {
        aggregate.downbeat.phase_correct_count as f64 / aggregate.downbeat.scored_tracks as f64
    });
    aggregate.meter.scored_tracks = groups.iter().map(|group| group.meter.scored_tracks).sum();
    aggregate.meter.correct_count = groups.iter().map(|group| group.meter.correct_count).sum();
    aggregate.meter.unknown_count = groups.iter().map(|group| group.meter.unknown_count).sum();
    aggregate.meter.wrong_count = groups.iter().map(|group| group.meter.wrong_count).sum();
    aggregate.meter.correct_rate = (aggregate.meter.scored_tracks > 0)
        .then(|| aggregate.meter.correct_count as f64 / aggregate.meter.scored_tracks as f64);
    aggregate.meter.unknown_rate = (aggregate.meter.scored_tracks > 0)
        .then(|| aggregate.meter.unknown_count as f64 / aggregate.meter.scored_tracks as f64);
    aggregate.meter.status = format!("{} groups", groups.len());
    aggregate
}

fn group_tracks<'a>(
    tracks: impl Iterator<Item = &'a TrackEvaluation>,
    key: impl Fn(&TrackEvaluation) -> String,
) -> BTreeMap<String, GroupMetrics> {
    let mut groups: BTreeMap<String, Vec<&GroupMetrics>> = BTreeMap::new();
    for track in tracks {
        groups.entry(key(track)).or_default().push(&track.metrics);
    }
    groups
        .into_iter()
        .map(|(key, values)| (key, aggregate_groups(values.into_iter())))
        .collect()
}

fn transform_metrics(
    specs: &[FixtureRecord],
    normalized: &BTreeMap<String, NormalizedAnalysis>,
) -> BTreeMap<String, TransformMetrics> {
    let mut result = BTreeMap::new();
    for spec in specs.iter().filter(|spec| spec.spec.base_id.is_some()) {
        let Some(base_id) = spec.spec.base_id.as_deref() else {
            continue;
        };
        let (Some(base), Some(transformed)) =
            (normalized.get(base_id), normalized.get(&spec.spec.id))
        else {
            continue;
        };
        let base_times = base
            .beats
            .iter()
            .map(|beat| beat.time_micros)
            .collect::<Vec<_>>();
        let transformed_times = transformed
            .beats
            .iter()
            .map(|beat| beat.time_micros)
            .collect::<Vec<_>>();
        let pairs = match_errors(&base_times, &transformed_times, BEAT_MATCH_WINDOW);
        let displacement = pairs
            .iter()
            .map(|(_, _, error)| error.as_secs_f64() * 1_000.0)
            .collect::<Vec<_>>();
        let tempo_changes = primary_tempo(base) != primary_tempo(transformed);
        let downbeat_changes = downbeat_signature(base) != downbeat_signature(transformed);
        let meter_changes = base.resolved_meter != transformed.resolved_meter;
        let confidence = mean(
            &pairs
                .iter()
                .map(|(base_index, transformed_index, _)| {
                    f64::from(
                        transformed.beats[*transformed_index].timing_confidence
                            - base.beats[*base_index].timing_confidence,
                    )
                })
                .collect::<Vec<_>>(),
        );
        let key = spec.spec.transform.label().to_owned();
        let entry = result
            .entry(key.clone())
            .or_insert_with(|| TransformMetrics {
                transform: key,
                ..TransformMetrics::default()
            });
        entry.tracks += 1;
        entry.matched_beats += pairs.len();
        entry.missing_beats += base.beats.len().saturating_sub(pairs.len());
        entry.extra_beats += transformed.beats.len().saturating_sub(pairs.len());
        entry.displacement_ms.extend(displacement.iter().copied());
        entry.mean_beat_displacement_ms = mean(&entry.displacement_ms);
        entry.p95_beat_displacement_ms = percentile(&entry.displacement_ms, 0.95);
        entry.tempo_interpretation_changes += usize::from(tempo_changes);
        entry.downbeat_changes += usize::from(downbeat_changes);
        entry.meter_changes += usize::from(meter_changes);
        entry.confidence_change = combine_means(entry.confidence_change, confidence, entry.tracks);
    }
    result
}

fn calibration(tracks: &[TrackEvaluation]) -> Vec<CalibrationBin> {
    (0..5)
        .map(|index| {
            let lower = index as f32 * 0.2;
            let upper = if index == 4 { 1.01 } else { lower + 0.2 };
            let mut confidence = Vec::new();
            let mut errors = Vec::new();
            let mut matched_count = 0;
            let mut unmatched_predicted_count = 0;
            for track in tracks {
                let predictions = track
                    .wotoha
                    .beats
                    .iter()
                    .map(|beat| (beat.time_micros, beat.timing_confidence))
                    .collect::<Vec<_>>();
                let pairs = match_errors(
                    &track.truth.beat_times_micros,
                    &predictions.iter().map(|beat| beat.0).collect::<Vec<_>>(),
                    BEAT_MATCH_WINDOW,
                );
                let matched_predictions = pairs
                    .iter()
                    .map(|(_, predicted_index, _)| *predicted_index)
                    .collect::<BTreeSet<_>>();
                for (truth_index, predicted_index, error) in pairs {
                    let confidence_value = predictions[predicted_index].1;
                    if confidence_value >= lower && confidence_value < upper {
                        matched_count += 1;
                        confidence.push(f64::from(confidence_value));
                        errors.push(error.as_secs_f64() * 1_000.0);
                    }
                    let _ = truth_index;
                }
                for (index, (_, confidence_value)) in predictions.iter().enumerate() {
                    if *confidence_value >= lower
                        && *confidence_value < upper
                        && !matched_predictions.contains(&index)
                    {
                        unmatched_predicted_count += 1;
                    }
                }
            }
            CalibrationBin {
                lower,
                upper: upper.min(1.0),
                observations: matched_count + unmatched_predicted_count,
                matched_count,
                unmatched_predicted_count,
                match_rate: (matched_count + unmatched_predicted_count > 0).then(|| {
                    matched_count as f64 / (matched_count + unmatched_predicted_count) as f64
                }),
                mean_confidence: mean(&confidence),
                mean_absolute_error_ms: mean(&errors),
            }
        })
        .collect()
}

fn variable_tempo_metrics(tracks: &[TrackEvaluation]) -> VariableTempoMetrics {
    let variable = tracks
        .iter()
        .filter(|track| {
            track.truth.tempo_segments.len() > 1
                || track
                    .truth
                    .tempo_segments
                    .first()
                    .is_some_and(|segment| (segment.start_bpm - segment.end_bpm).abs() > 0.01)
        })
        .collect::<Vec<_>>();
    let mut local_errors = Vec::new();
    let mut phase_errors = Vec::new();
    let mut change_delays = Vec::new();
    for track in &variable {
        for window in track.wotoha.beats.windows(2) {
            let interval = window[1].time_micros.saturating_sub(window[0].time_micros);
            if interval == 0 {
                continue;
            }
            let midpoint = (window[0].time_micros + window[1].time_micros) / 2;
            let truth_bpm = truth_bpm_at(&track.truth.tempo_segments, midpoint);
            if let Some(truth_bpm) = truth_bpm {
                local_errors.push(((60_000_000.0 / interval as f64) - f64::from(truth_bpm)).abs());
            }
        }
        let errors = match_errors(
            &track.truth.beat_times_micros,
            &track
                .wotoha
                .beats
                .iter()
                .map(|beat| beat.time_micros)
                .collect::<Vec<_>>(),
            Duration::from_millis(300),
        );
        phase_errors.extend(
            errors
                .into_iter()
                .map(|(_, _, error)| error.as_secs_f64() * 1_000.0),
        );
        for segment in track.truth.tempo_segments.iter().filter(|segment| {
            segment.start_micros > 0 && (segment.start_bpm - segment.end_bpm).abs() < 0.01
        }) {
            if let Some((time, _)) = track
                .wotoha
                .beats
                .windows(2)
                .map(|window| {
                    let midpoint = (window[0].time_micros + window[1].time_micros) / 2;
                    let interval = window[1].time_micros.saturating_sub(window[0].time_micros);
                    (midpoint, interval)
                })
                .find(|(time, interval)| {
                    *time >= segment.start_micros
                        && *interval > 0
                        && ((60_000_000.0 / *interval as f64) - f64::from(segment.start_bpm)).abs()
                            / f64::from(segment.start_bpm)
                            < 0.05
                })
            {
                change_delays.push(time.saturating_sub(segment.start_micros) as f64 / 1_000.0);
            }
        }
    }
    VariableTempoMetrics {
        tracks: variable.len(),
        local_tempo_mae_bpm: mean(&local_errors),
        phase_drift_p95_ms: percentile(&phase_errors, 0.95),
        change_tracking_delay_ms: mean(&change_delays),
    }
}

fn compare_external(
    document: &ExternalObservationDocument,
    manifest: &SyntheticCorpusManifest,
    tracks: &[TrackEvaluation],
) -> Result<ExternalReport, LabError> {
    let records = manifest
        .fixtures
        .iter()
        .map(|record| (record.spec.id.as_str(), record))
        .collect::<BTreeMap<_, _>>();
    let track_map = tracks
        .iter()
        .map(|track| (track.sample_id.as_str(), track))
        .collect::<BTreeMap<_, _>>();
    let mut by_observer = BTreeMap::new();
    let mut complete_observations = 0;
    let mut incomplete_observations = 0;
    for observation in &document.observations {
        let Some(record) = records.get(observation.sample_id.as_str()) else {
            return Err(LabError::UnknownSample(observation.sample_id.clone()));
        };
        if record.audio_sha256 != observation.audio_sha256 {
            return Err(LabError::HashMismatch {
                sample_id: observation.sample_id.clone(),
                expected: record.audio_sha256.clone(),
                actual: observation.audio_sha256.clone(),
            });
        }
        let track = track_map
            .get(observation.sample_id.as_str())
            .expect("track was evaluated");
        let external = normalized_external(observation);
        let wotoha_vs_external = metrics_between_predictions(&track.wotoha, &external);
        let external_vs_truth = external_metrics_for(&record.truth, &external);
        let settings = observation_settings_key(&observation.analysis_settings);
        let key = format!(
            "{}@{} [{}]",
            observation.observer.product, observation.observer.version, settings
        );
        let entry = by_observer
            .entry(key)
            .or_insert_with(|| ExternalObserverReport {
                product: observation.observer.product.clone(),
                version: observation.observer.version.clone(),
                settings: settings.clone(),
                ..ExternalObserverReport::default()
            });
        entry.observations += 1;
        if observation.observed.analysis_complete {
            entry.complete_observations += 1;
            complete_observations += 1;
        } else {
            entry.incomplete_observations += 1;
            incomplete_observations += 1;
        }
        entry.beat_observations +=
            usize::from(observation.observed.beatgrid_times_micros.is_some());
        entry.tempo_observations += usize::from(observation.observed.reported_bpm.is_some());
        entry.meter_observations += usize::from(observation.observed.meter.is_some());
        entry.downbeat_observations += usize::from(observation.observed.downbeat_indices.is_some());
        entry.grid_phase_observations +=
            usize::from(observation.observed.grid_phase_micros.is_some());
        entry.wotoha_vs_truth = merge_group(&entry.wotoha_vs_truth, &track.metrics);
        entry.external_vs_truth = merge_group(&entry.external_vs_truth, &external_vs_truth);
        entry.wotoha_vs_external = merge_group(&entry.wotoha_vs_external, &wotoha_vs_external);
    }
    Ok(ExternalReport {
        observations: document.observations.len(),
        complete_observations,
        incomplete_observations,
        by_observer,
    })
}

fn normalized_external(observation: &ExternalAnalysisObservation) -> NormalizedExternalAnalysis {
    let beats = observation
        .observed
        .beatgrid_times_micros
        .clone()
        .map(|times| {
            times
                .into_iter()
                .enumerate()
                .map(|(index, time_micros)| NormalizedBeat {
                    time_micros,
                    timing_confidence: 1.0,
                    beat_model_score: None,
                    onset_support: None,
                    low_frequency_support: None,
                    downbeat_evidence: observation
                        .observed
                        .downbeat_indices
                        .as_ref()
                        .and_then(|indices| indices.contains(&index).then_some(1.0)),
                })
                .collect()
        });
    NormalizedExternalAnalysis {
        analyzer: format!(
            "external:{}@{}",
            observation.observer.product, observation.observer.version
        ),
        duration_micros: observation
            .timing
            .observed_duration_micros
            .unwrap_or_default(),
        beats,
        reported_bpm: observation.observed.reported_bpm,
        downbeat_indices: observation.observed.downbeat_indices.clone(),
        grid_phase_micros: observation.observed.grid_phase_micros,
        meter: observation.observed.meter,
        analysis_complete: observation.observed.analysis_complete,
    }
}

fn external_field_available<T>(field: &Option<T>, complete: bool, is_empty: bool) -> bool {
    field.is_some() && (complete || !is_empty)
}

fn external_metrics_for(
    truth: &AnalysisGroundTruth,
    prediction: &NormalizedExternalAnalysis,
) -> GroupMetrics {
    let beats_available = external_field_available(
        &prediction.beats,
        prediction.analysis_complete,
        prediction.beats.as_ref().is_some_and(Vec::is_empty),
    );
    let downbeats_available = external_field_available(
        &prediction.downbeat_indices,
        prediction.analysis_complete,
        prediction
            .downbeat_indices
            .as_ref()
            .is_some_and(Vec::is_empty),
    );
    let beat_values = prediction.beats.as_ref().map(|beats| {
        beats
            .iter()
            .map(|beat| beat.time_micros)
            .collect::<Vec<_>>()
    });
    GroupMetrics {
        tracks: 1,
        beat: if beats_available {
            beat_metrics(
                &truth.beat_times_micros,
                beat_values.as_deref().unwrap_or_default(),
            )
        } else {
            BeatMetrics::default()
        },
        tempo: external_tempo_metrics(truth.tempo.as_ref(), prediction.reported_bpm),
        grid_phase: if prediction.grid_phase_micros.is_some() {
            phase_metrics_at(truth, prediction.grid_phase_micros)
        } else if beats_available {
            phase_metrics_at(
                truth,
                beat_values
                    .as_deref()
                    .and_then(|beats| beats.first().copied()),
            )
        } else {
            PhaseMetrics::default()
        },
        downbeat: if downbeats_available && beats_available {
            downbeat_metrics_from_indices(
                truth,
                prediction.downbeat_indices.as_deref().unwrap_or_default(),
            )
        } else {
            DownbeatMetrics::default()
        },
        meter: prediction.meter.map_or_else(
            || meter_metrics_value(None, None),
            |meter| meter_metrics_value(truth.meter, Some(meter)),
        ),
    }
}

fn external_tempo_metrics(truth: Option<&TempoTruth>, bpm: Option<f32>) -> TempoMetrics {
    let Some(bpm) = bpm else {
        return TempoMetrics::default();
    };
    tempo_metrics(
        truth,
        &[NormalizedTempoHypothesis {
            bpm,
            relative_weight: 1.0,
            relation: "primary".into(),
        }],
    )
}

fn metrics_between_predictions(
    wotoha: &NormalizedAnalysis,
    external: &NormalizedExternalAnalysis,
) -> GroupMetrics {
    let beats_available = external_field_available(
        &external.beats,
        external.analysis_complete,
        external.beats.as_ref().is_some_and(Vec::is_empty),
    );
    let downbeats_available = external_field_available(
        &external.downbeat_indices,
        external.analysis_complete,
        external
            .downbeat_indices
            .as_ref()
            .is_some_and(Vec::is_empty),
    );
    let external_beats = external.beats.as_ref().map(|beats| {
        beats
            .iter()
            .map(|beat| beat.time_micros)
            .collect::<Vec<_>>()
    });
    let external_phase = external.grid_phase_micros.or_else(|| {
        beats_available
            .then(|| {
                external_beats
                    .as_deref()
                    .and_then(|beats| beats.first().copied())
            })
            .flatten()
    });
    let period = external
        .reported_bpm
        .map(|bpm| (60_000_000.0 / f64::from(bpm)).round() as u64)
        .or_else(|| {
            external_beats
                .as_deref()
                .and_then(|beats| beats.windows(2).next().map(|window| window[1] - window[0]))
        });
    GroupMetrics {
        tracks: 1,
        beat: if beats_available {
            beat_metrics(
                external_beats.as_deref().unwrap_or_default(),
                &wotoha
                    .beats
                    .iter()
                    .map(|beat| beat.time_micros)
                    .collect::<Vec<_>>(),
            )
        } else {
            BeatMetrics::default()
        },
        tempo: external_tempo_metrics(
            external
                .reported_bpm
                .map(|bpm| TempoTruth {
                    primary_bpm: bpm,
                    valid_alternates_bpm: Vec::new(),
                })
                .as_ref(),
            primary_tempo_bpm(wotoha),
        ),
        grid_phase: match (wotoha.beats.first(), external_phase, period) {
            (Some(predicted), Some(reference), Some(period)) if period > 0 => {
                phase_metrics_between(predicted.time_micros, reference, period)
            }
            _ => PhaseMetrics::default(),
        },
        downbeat: if downbeats_available && beats_available {
            downbeat_metrics_from_indices(
                &AnalysisGroundTruth {
                    tempo: None,
                    meter: external.meter,
                    beat_times_micros: external_beats.clone().unwrap_or_default(),
                    downbeats: external.downbeat_indices.clone().unwrap_or_default(),
                    tempo_segments: Vec::new(),
                    duration_micros: external.duration_micros,
                },
                &downbeat_signature(wotoha),
            )
        } else {
            DownbeatMetrics::default()
        },
        meter: if external.meter.is_some() {
            meter_metrics_value(external.meter, wotoha.resolved_meter)
        } else {
            MeterMetrics::default()
        },
    }
}
fn merge_group(left: &GroupMetrics, right: &GroupMetrics) -> GroupMetrics {
    if left.tracks == 0 {
        return right.clone();
    }
    if right.tracks == 0 {
        return left.clone();
    }
    aggregate_groups([left, right].into_iter())
}

fn summarize_failures(
    tracks: &[TrackEvaluation],
) -> (
    BTreeMap<String, usize>,
    BTreeMap<String, usize>,
    BTreeMap<String, usize>,
) {
    let mut half = BTreeMap::new();
    let mut downbeat = BTreeMap::new();
    let mut clusters = BTreeMap::new();
    for track in tracks {
        for failure in &track.failure_clusters {
            *clusters.entry(failure.clone()).or_default() += 1;
            if failure.contains("half") || failure.contains("double") {
                *half.entry(failure.clone()).or_default() += 1;
            }
            if failure.contains("downbeat") {
                *downbeat.entry(failure.clone()).or_default() += 1;
            }
        }
    }
    (half, downbeat, clusters)
}

fn failure_clusters(
    truth: &AnalysisGroundTruth,
    prediction: &NormalizedAnalysis,
    metrics: &GroupMetrics,
) -> Vec<String> {
    let mut failures = Vec::new();
    if metrics.tempo.relation_error.as_deref() == Some("half_time") {
        failures.push("half_time_selection".into());
    }
    if metrics.tempo.relation_error.as_deref() == Some("double_time") {
        failures.push("double_time_selection".into());
    }
    if metrics.grid_phase.correct == Some(false) && metrics.tempo.primary_correct == Some(true) {
        failures.push("correct_tempo_wrong_grid_phase".into());
    }
    if metrics.downbeat.phase_correct == Some(false) && metrics.beat.matched > 0 {
        failures.push("correct_beats_wrong_downbeat".into());
    }
    if metrics.meter.status == "unknown" {
        failures.push("meter_unresolved".into());
    }
    if metrics
        .beat
        .recall_at_tolerance
        .get("40ms")
        .copied()
        .unwrap_or_default()
        < 0.5
    {
        failures.push(
            if truth.beat_times_micros.len() < 8 {
                "sparse_transient_failure"
            } else {
                "beat_timeline_failure"
            }
            .into(),
        );
    }
    if prediction
        .beats
        .iter()
        .any(|beat| beat.timing_confidence >= 0.8)
        && metrics
            .beat
            .recall_at_tolerance
            .get("40ms")
            .copied()
            .unwrap_or_default()
            < 0.5
    {
        failures.push("confidence_high_despite_wrong_timeline".into());
    }
    failures
}

fn primary_tempo(analysis: &NormalizedAnalysis) -> Option<i32> {
    analysis
        .tempo_hypotheses
        .first()
        .map(|hypothesis| (hypothesis.bpm * 100.0).round() as i32)
}
fn primary_tempo_bpm(analysis: &NormalizedAnalysis) -> Option<f32> {
    analysis
        .tempo_hypotheses
        .first()
        .map(|hypothesis| hypothesis.bpm)
}
fn downbeat_signature(analysis: &NormalizedAnalysis) -> Vec<usize> {
    analysis
        .beats
        .iter()
        .enumerate()
        .filter_map(|(index, beat)| {
            (beat.downbeat_evidence.unwrap_or_default() >= 0.5).then_some(index)
        })
        .collect()
}
fn relation_label(bpm: f32, truth: f32) -> String {
    let ratio = bpm / truth;
    if (ratio - 1.0).abs() < 0.005 {
        "canonical"
    } else if (ratio - 0.5).abs() < 0.01 {
        "half_time"
    } else if (ratio - 2.0).abs() < 0.02 {
        "double_time"
    } else {
        "alternative"
    }
    .into()
}
fn truth_bpm_at(segments: &[TempoSegmentTruth], time: u64) -> Option<f32> {
    segments
        .iter()
        .find(|segment| (segment.start_micros..segment.end_micros).contains(&time))
        .map(|segment| {
            let fraction = (time.saturating_sub(segment.start_micros)) as f32
                / segment
                    .end_micros
                    .saturating_sub(segment.start_micros)
                    .max(1) as f32;
            segment.start_bpm + (segment.end_bpm - segment.start_bpm) * fraction.clamp(0.0, 1.0)
        })
}
fn tempo_range(bpm: Option<f32>) -> String {
    bpm.map(|bpm| {
        match bpm {
            bpm if bpm < 90.0 => "<90",
            bpm if bpm < 120.0 => "90-119.99",
            bpm if bpm < 140.0 => "120-139.99",
            _ => "140+",
        }
        .into()
    })
    .unwrap_or_else(|| "variable".into())
}
fn safe_ratio(numerator: f64, denominator: f64) -> f64 {
    if denominator > 0.0 {
        numerator / denominator
    } else {
        0.0
    }
}
fn mean(values: &[f64]) -> Option<f64> {
    (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
}
fn percentile(values: &[f64], percentile: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut values = values.to_vec();
    values.sort_by(f64::total_cmp);
    let index = ((values.len() - 1) as f64 * percentile).round() as usize;
    values.get(index).copied()
}
fn weighted_metric(values: impl Iterator<Item = (Option<f64>, usize)>) -> Option<f64> {
    let mut numerator = 0.0;
    let mut denominator = 0;
    for (value, weight) in values {
        if let Some(value) = value {
            numerator += value * weight as f64;
            denominator += weight;
        }
    }
    (denominator > 0).then_some(numerator / denominator as f64)
}
fn combine_means(left: Option<f64>, right: Option<f64>, count: usize) -> Option<f64> {
    match (left, right) {
        (None, right) => right,
        (left, None) => left,
        (Some(left), Some(right)) => Some(left + (right - left) / count.max(1) as f64),
    }
}
fn format_opt(value: Option<f64>) -> String {
    value
        .map(|value| format!("{value:.3}"))
        .unwrap_or_else(|| "n/a".into())
}

fn bool_fraction(value: bool) -> f64 {
    if value { 1.0 } else { 0.0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_serialization_round_trip_preserves_optional_absence() {
        let observation = ExternalAnalysisObservation {
            schema_version: OBSERVATION_SCHEMA_VERSION,
            sample_id: "sample".into(),
            audio_sha256: "a".repeat(64),
            observer: ObserverIdentity {
                product: "observer".into(),
                version: "1".into(),
                platform: None,
            },
            analysis_settings: ObservationSettings {
                beat_grid_enabled: None,
                tempo_range_bpm: None,
                meter_mode: None,
                key_mode: None,
            },
            observed: ObservedAnalysis {
                reported_bpm: None,
                beatgrid_times_micros: None,
                downbeat_indices: None,
                grid_phase_micros: None,
                musical_key: None,
                meter: None,
                analysis_complete: false,
            },
            timing: ObservationTiming::default(),
            notes: Vec::new(),
        };
        let encoded = serde_json::to_string(&observation).unwrap();
        let decoded: ExternalAnalysisObservation = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.observed.reported_bpm, None);
        assert_eq!(decoded.observed.beatgrid_times_micros, None);
    }

    #[test]
    fn observation_version_is_rejected() {
        let document = ExternalObservationDocument {
            schema_version: OBSERVATION_SCHEMA_VERSION + 1,
            observations: Vec::new(),
        };
        assert!(document.validate().is_err());
    }

    #[test]
    fn beat_matching_is_one_to_one_and_metrics_are_stable() {
        let truth = vec![0, 500_000, 1_000_000];
        let predicted = vec![5_000, 10_000, 505_000, 2_000_000];
        let pairs = match_errors(&truth, &predicted, Duration::from_millis(20));
        assert_eq!(pairs.len(), 2);
        let metrics = beat_metrics(&truth, &predicted);
        assert!(metrics.mae_ms.is_some());
        assert!(metrics.p50_ms.is_some());
        assert!(metrics.p95_ms.is_some());
        for tolerance in ["10ms", "20ms", "40ms", "70ms"] {
            assert!(metrics.precision_at_tolerance.contains_key(tolerance));
            assert!(metrics.recall_at_tolerance.contains_key(tolerance));
        }
    }

    #[test]
    fn tempo_credit_distinguishes_half_time_from_missing_correct_hypothesis() {
        let truth = TempoTruth {
            primary_bpm: 128.0,
            valid_alternates_bpm: vec![64.0],
        };
        let hypotheses = vec![
            NormalizedTempoHypothesis {
                bpm: 64.0,
                relative_weight: 1.0,
                relation: "primary".into(),
            },
            NormalizedTempoHypothesis {
                bpm: 128.0,
                relative_weight: 0.8,
                relation: "double_time".into(),
            },
        ];
        let metrics = tempo_metrics(Some(&truth), &hypotheses);
        assert_eq!(metrics.relation_error.as_deref(), Some("half_time"));
        assert_eq!(metrics.correct_hypothesis_top_n.get("3"), Some(&true));
        let missing = tempo_metrics(
            Some(&truth),
            &[NormalizedTempoHypothesis {
                bpm: 64.0,
                relative_weight: 1.0,
                relation: "primary".into(),
            }],
        );
        assert_eq!(missing.correct_hypothesis_top_n.get("3"), Some(&false));
    }

    #[test]
    fn variable_tempo_truth_is_not_collapsed_in_the_schema() {
        let spec = spec(
            "ramp",
            FixtureFamily::TempoDrift,
            TempoProfile::LinearRamp {
                start_bpm: 120.0,
                end_bpm: 124.0,
            },
            EventStyle::Standard,
            4,
            0,
            1,
        );
        let fixture = generate_fixture(&spec).unwrap();
        assert_eq!(fixture.truth.tempo, None);
        assert_eq!(fixture.truth.tempo_segments[0].start_bpm, 120.0);
        assert_eq!(fixture.truth.tempo_segments[0].end_bpm, 124.0);
    }

    #[test]
    fn synthetic_generation_is_deterministic_and_hash_identifies_audio() {
        let spec = spec(
            "deterministic",
            FixtureFamily::ConstantTempo,
            TempoProfile::Constant { bpm: 120.0 },
            EventStyle::Standard,
            4,
            0,
            42,
        );
        let first = generate_fixture(&spec).unwrap();
        let second = generate_fixture(&spec).unwrap();
        assert_eq!(first.audio_sha256, second.audio_sha256);
        assert_eq!(first.audio, second.audio);
    }

    #[test]
    fn transform_fixture_keeps_identity_to_base_truth() {
        let mut spec = spec(
            "gain",
            FixtureFamily::Transform,
            TempoProfile::Constant { bpm: 120.0 },
            EventStyle::Standard,
            4,
            0,
            42,
        );
        spec.transform = TransformKind::Gain { factor: 0.25 };
        spec.base_id = Some("base".into());
        let fixture = generate_fixture(&spec).unwrap();
        let base_spec = FixtureSpec {
            transform: TransformKind::None,
            base_id: None,
            id: "base".into(),
            ..spec.clone()
        };
        let base = generate_fixture(&base_spec).unwrap();
        assert_eq!(
            fixture.truth.beat_times_micros.len(),
            base.truth.beat_times_micros.len()
        );
        assert_ne!(fixture.audio_sha256, base.audio_sha256);
    }

    #[test]
    fn external_hash_mismatch_is_rejected() {
        let manifest = generate_default_manifest(1).unwrap();
        let first = &manifest.fixtures[0];
        let observation = ExternalAnalysisObservation {
            schema_version: 1,
            sample_id: first.spec.id.clone(),
            audio_sha256: "b".repeat(64),
            observer: ObserverIdentity {
                product: "observer".into(),
                version: "1".into(),
                platform: None,
            },
            analysis_settings: ObservationSettings {
                beat_grid_enabled: None,
                tempo_range_bpm: None,
                meter_mode: None,
                key_mode: None,
            },
            observed: ObservedAnalysis {
                reported_bpm: None,
                beatgrid_times_micros: None,
                downbeat_indices: None,
                grid_phase_micros: None,
                musical_key: None,
                meter: None,
                analysis_complete: false,
            },
            timing: ObservationTiming::default(),
            notes: Vec::new(),
        };
        let result = compare_external(
            &ExternalObservationDocument {
                schema_version: 1,
                observations: vec![observation],
            },
            &manifest,
            &[],
        );
        assert!(result.is_err());
    }

    #[test]
    fn valid_external_observation_is_grouped_by_observer_version() {
        let spec = spec(
            "external-sample",
            FixtureFamily::ConstantTempo,
            TempoProfile::Constant { bpm: 120.0 },
            EventStyle::Standard,
            4,
            0,
            99,
        );
        let fixture = generate_fixture(&spec).unwrap();
        let record = FixtureRecord {
            spec: fixture.spec.clone(),
            truth: fixture.truth.clone(),
            audio_sha256: fixture.audio_sha256.clone(),
        };
        let manifest = SyntheticCorpusManifest {
            schema_version: LAB_SCHEMA_VERSION,
            split: "development".into(),
            seed: 99,
            fixtures: vec![record],
        };
        let observation = ExternalAnalysisObservation {
            schema_version: OBSERVATION_SCHEMA_VERSION,
            sample_id: fixture.spec.id.clone(),
            audio_sha256: fixture.audio_sha256.clone(),
            observer: ObserverIdentity {
                product: "neutral-observer".into(),
                version: "1.0".into(),
                platform: Some("test".into()),
            },
            analysis_settings: ObservationSettings {
                beat_grid_enabled: Some(true),
                tempo_range_bpm: None,
                meter_mode: None,
                key_mode: None,
            },
            observed: ObservedAnalysis {
                reported_bpm: Some(120.0),
                beatgrid_times_micros: Some(fixture.truth.beat_times_micros.clone()),
                downbeat_indices: Some(fixture.truth.downbeats.clone()),
                grid_phase_micros: fixture.truth.beat_times_micros.first().copied(),
                musical_key: None,
                meter: Some(4),
                analysis_complete: true,
            },
            timing: ObservationTiming {
                analysis_elapsed_millis: Some(1),
                observed_duration_micros: Some(fixture.truth.duration_micros),
            },
            notes: Vec::new(),
        };
        let report = evaluate_manifest(
            &manifest,
            Some(&ExternalObservationDocument {
                schema_version: OBSERVATION_SCHEMA_VERSION,
                observations: vec![observation],
            }),
            EvaluationOptions {
                mode: AnalyzerMode::Classical,
                split: "development".into(),
            },
        )
        .unwrap();
        let observer = report.external.by_observer.values().next().unwrap();
        assert_eq!(report.external.observations, 1);
        assert_eq!(observer.external_vs_truth.tracks, 1);
        assert_eq!(observer.wotoha_vs_external.tracks, 1);
    }

    #[test]
    fn phase_and_downbeat_errors_are_separate_from_beat_matching() {
        let truth = AnalysisGroundTruth {
            tempo: Some(TempoTruth {
                primary_bpm: 120.0,
                valid_alternates_bpm: Vec::new(),
            }),
            meter: Some(4),
            beat_times_micros: vec![0, 500_000, 1_000_000],
            downbeats: vec![0],
            tempo_segments: Vec::new(),
            duration_micros: 2_000_000,
        };
        let prediction = NormalizedAnalysis {
            analyzer: "test".into(),
            duration_micros: 2_000_000,
            beats: vec![
                NormalizedBeat {
                    time_micros: 100_000,
                    timing_confidence: 0.8,
                    beat_model_score: None,
                    onset_support: None,
                    low_frequency_support: None,
                    downbeat_evidence: None,
                },
                NormalizedBeat {
                    time_micros: 600_000,
                    timing_confidence: 0.8,
                    beat_model_score: None,
                    onset_support: None,
                    low_frequency_support: None,
                    downbeat_evidence: Some(1.0),
                },
            ],
            tempo_hypotheses: vec![NormalizedTempoHypothesis {
                bpm: 120.0,
                relative_weight: 1.0,
                relation: "primary".into(),
            }],
            meter_hypotheses: Vec::new(),
            resolved_meter: None,
            resolved_meter_phase: None,
            structure: NormalizedStructure::default(),
            confidence: ConfidenceEvidence::default(),
            provenance: BTreeMap::new(),
        };
        let metrics = metrics_for(&truth, &prediction);
        assert_eq!(metrics.grid_phase.correct, Some(false));
        assert_eq!(metrics.downbeat.first_offset_beats, None);
        assert_eq!(metrics.downbeat.phase_correct, None);
        assert_eq!(metrics.downbeat.scored_tracks, 0);
        assert_eq!(metrics.meter.status, "unknown");
        assert_eq!(metrics.meter.correct_count, 0);

        let mut resolved_prediction = prediction.clone();
        resolved_prediction.resolved_meter = Some(4);
        resolved_prediction.beats[1].downbeat_evidence = Some(1.0);
        let resolved_metrics = metrics_for(&truth, &resolved_prediction);
        assert_eq!(resolved_metrics.downbeat.first_offset_beats, Some(1));
        assert_eq!(resolved_metrics.downbeat.phase_correct, Some(false));
        assert_eq!(resolved_metrics.downbeat.scored_tracks, 1);
    }

    fn truth_with_meter(meter: Option<u8>) -> AnalysisGroundTruth {
        AnalysisGroundTruth {
            tempo: Some(TempoTruth {
                primary_bpm: 120.0,
                valid_alternates_bpm: vec![60.0, 240.0],
            }),
            meter,
            beat_times_micros: vec![0, 500_000, 1_000_000, 1_500_000],
            downbeats: vec![0],
            tempo_segments: vec![TempoSegmentTruth {
                start_micros: 0,
                end_micros: 2_000_000,
                start_bpm: 120.0,
                end_bpm: 120.0,
            }],
            duration_micros: 2_000_000,
        }
    }

    fn observation_with(
        sample_id: &str,
        beats: Option<Vec<u64>>,
        downbeats: Option<Vec<usize>>,
        bpm: Option<f32>,
        meter: Option<u8>,
        phase: Option<u64>,
        complete: bool,
    ) -> ExternalAnalysisObservation {
        ExternalAnalysisObservation {
            schema_version: OBSERVATION_SCHEMA_VERSION,
            sample_id: sample_id.into(),
            audio_sha256: "a".repeat(64),
            observer: ObserverIdentity {
                product: "observer".into(),
                version: "1".into(),
                platform: Some("test".into()),
            },
            analysis_settings: ObservationSettings {
                beat_grid_enabled: Some(true),
                tempo_range_bpm: None,
                meter_mode: None,
                key_mode: None,
            },
            observed: ObservedAnalysis {
                reported_bpm: bpm,
                beatgrid_times_micros: beats,
                downbeat_indices: downbeats,
                grid_phase_micros: phase,
                musical_key: None,
                meter,
                analysis_complete: complete,
            },
            timing: ObservationTiming {
                analysis_elapsed_millis: None,
                observed_duration_micros: Some(2_000_000),
            },
            notes: Vec::new(),
        }
    }

    fn normalized_beats(times: &[u64]) -> Vec<NormalizedBeat> {
        times
            .iter()
            .map(|time_micros| NormalizedBeat {
                time_micros: *time_micros,
                timing_confidence: 0.9,
                beat_model_score: None,
                onset_support: None,
                low_frequency_support: None,
                downbeat_evidence: None,
            })
            .collect()
    }

    fn normalized_prediction(times: &[u64]) -> NormalizedAnalysis {
        NormalizedAnalysis {
            analyzer: "test".into(),
            duration_micros: 2_000_000,
            beats: normalized_beats(times),
            tempo_hypotheses: vec![NormalizedTempoHypothesis {
                bpm: 120.0,
                relative_weight: 1.0,
                relation: "primary".into(),
            }],
            meter_hypotheses: Vec::new(),
            resolved_meter: Some(4),
            resolved_meter_phase: Some(0),
            structure: NormalizedStructure::default(),
            confidence: ConfidenceEvidence::default(),
            provenance: BTreeMap::new(),
        }
    }

    #[test]
    fn meter_metrics_uses_canonical_resolved_meter_and_preserves_unknown() {
        use wotoha_core::analysis::{MeterHypothesis, RhythmAnalysis, UnitInterval};

        let mut analysis = TrackAnalysisV2::unanalyzed(Duration::from_secs(2));
        analysis.rhythm = RhythmAnalysis::new(
            Vec::new(),
            Vec::new(),
            vec![
                MeterHypothesis::new(4, 0, UnitInterval::new(0.85).unwrap()).unwrap(),
                MeterHypothesis::new(3, 0, UnitInterval::new(0.10).unwrap()).unwrap(),
            ],
        )
        .unwrap();
        let resolved = normalize_v2(&analysis);
        assert_eq!(resolved.resolved_meter, Some(4));
        assert_eq!(
            meter_metrics(&truth_with_meter(Some(4)), &resolved).status,
            "correct"
        );

        for scores in [(0.55, 0.50), (0.70, 0.62)] {
            analysis.rhythm.meter_hypotheses = vec![
                MeterHypothesis::new(4, 0, UnitInterval::new(scores.0).unwrap()).unwrap(),
                MeterHypothesis::new(3, 0, UnitInterval::new(scores.1).unwrap()).unwrap(),
            ];
            let resolved = normalize_v2(&analysis);
            assert_eq!(resolved.resolved_meter, None);
            assert_eq!(
                meter_metrics(&truth_with_meter(Some(4)), &resolved).status,
                "unknown"
            );
        }

        analysis.rhythm.meter_hypotheses = vec![
            MeterHypothesis::new(4, 0, UnitInterval::new(0.90).unwrap()).unwrap(),
            MeterHypothesis::new(3, 0, UnitInterval::new(0.05).unwrap()).unwrap(),
        ];
        let resolved = normalize_v2(&analysis);
        assert_eq!(
            meter_metrics(&truth_with_meter(Some(3)), &resolved).status,
            "wrong"
        );
        assert_eq!(resolved.meter_hypotheses.len(), 2);
    }

    #[test]
    fn external_none_empty_and_incomplete_fields_are_not_collapsed() {
        let truth = truth_with_meter(Some(4));
        let absent = normalized_external(&observation_with(
            "sample", None, None, None, None, None, true,
        ));
        let absent_metrics = external_metrics_for(&truth, &absent);
        assert_eq!(absent_metrics.beat.truth, 0);
        assert_eq!(absent_metrics.beat.predicted, 0);
        assert_eq!(absent_metrics.meter.status, "not_scored");

        let empty = normalized_external(&observation_with(
            "sample",
            Some(Vec::new()),
            Some(Vec::new()),
            None,
            None,
            None,
            true,
        ));
        let empty_metrics = external_metrics_for(&truth, &empty);
        assert_eq!(empty_metrics.beat.truth, truth.beat_times_micros.len());
        assert_eq!(empty_metrics.beat.predicted, 0);
        assert_eq!(empty_metrics.beat.recall_at_tolerance["40ms"], 0.0);
        assert_eq!(empty_metrics.downbeat.scored_tracks, 1);

        let incomplete = normalized_external(&observation_with(
            "sample",
            Some(Vec::new()),
            Some(Vec::new()),
            Some(120.0),
            None,
            None,
            false,
        ));
        let incomplete_metrics = external_metrics_for(&truth, &incomplete);
        assert_eq!(incomplete_metrics.beat.truth, 0);
        assert_eq!(incomplete_metrics.downbeat.scored_tracks, 0);
        assert_eq!(incomplete_metrics.tempo.scored_tracks, 1);
    }

    #[test]
    fn external_explicit_phase_and_downbeat_availability_are_scored_independently() {
        let truth = truth_with_meter(Some(4));
        let phase_only = normalized_external(&observation_with(
            "sample",
            None,
            None,
            Some(120.0),
            None,
            Some(0),
            true,
        ));
        let metrics = external_metrics_for(&truth, &phase_only);
        assert_eq!(metrics.grid_phase.scored_tracks, 1);
        assert_eq!(metrics.beat.truth, 0);
        assert_eq!(metrics.downbeat.scored_tracks, 0);

        let no_downbeats = normalized_external(&observation_with(
            "sample",
            Some(truth.beat_times_micros.clone()),
            None,
            None,
            None,
            None,
            true,
        ));
        assert_eq!(
            external_metrics_for(&truth, &no_downbeats)
                .downbeat
                .scored_tracks,
            0
        );
        let empty_downbeats = normalized_external(&observation_with(
            "sample",
            Some(truth.beat_times_micros.clone()),
            Some(Vec::new()),
            None,
            None,
            None,
            true,
        ));
        assert_eq!(
            external_metrics_for(&truth, &empty_downbeats)
                .downbeat
                .scored_tracks,
            1
        );
        assert_eq!(
            external_metrics_for(&truth, &empty_downbeats)
                .downbeat
                .phase_correct,
            Some(false)
        );
    }

    #[test]
    fn external_validation_rejects_duplicates_invalid_indices_and_invalid_settings() {
        let mut first = observation_with(
            "sample",
            Some(vec![0, 500_000]),
            Some(vec![0]),
            None,
            None,
            None,
            true,
        );
        first.audio_sha256 = "a".repeat(64);
        let duplicate = ExternalObservationDocument {
            schema_version: OBSERVATION_SCHEMA_VERSION,
            observations: vec![first.clone(), first],
        };
        assert!(duplicate.validate().is_err());

        let mut invalid_index = observation_with(
            "sample",
            Some(vec![0, 500_000]),
            Some(vec![2]),
            None,
            None,
            None,
            true,
        );
        invalid_index.audio_sha256 = "a".repeat(64);
        assert!(
            ExternalObservationDocument {
                schema_version: OBSERVATION_SCHEMA_VERSION,
                observations: vec![invalid_index],
            }
            .validate()
            .is_err()
        );

        let mut invalid_settings = observation_with("sample", None, None, None, None, None, true);
        invalid_settings.analysis_settings.tempo_range_bpm = Some((140.0, 100.0));
        assert!(
            ExternalObservationDocument {
                schema_version: OBSERVATION_SCHEMA_VERSION,
                observations: vec![invalid_settings],
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn different_observation_settings_are_grouped_without_silent_pooling() {
        let manifest = generate_default_manifest(7).unwrap();
        let first = &manifest.fixtures[0];
        let mut one = observation_with(
            &first.spec.id,
            Some(first.truth.beat_times_micros.clone()),
            Some(first.truth.downbeats.clone()),
            Some(first.truth.tempo.as_ref().unwrap().primary_bpm),
            Some(4),
            first.truth.beat_times_micros.first().copied(),
            true,
        );
        one.audio_sha256 = first.audio_sha256.clone();
        one.timing.observed_duration_micros = Some(first.truth.duration_micros);
        let mut two = one.clone();
        two.analysis_settings.tempo_range_bpm = Some((100.0, 200.0));
        let document = ExternalObservationDocument {
            schema_version: OBSERVATION_SCHEMA_VERSION,
            observations: vec![one, two],
        };
        let report = evaluate_manifest(
            &manifest,
            Some(&document),
            EvaluationOptions {
                mode: AnalyzerMode::Classical,
                split: "development".into(),
            },
        )
        .unwrap();
        assert_eq!(report.external.observations, 2);
        assert_eq!(report.external.by_observer.len(), 2);
    }

    #[test]
    fn aggregate_beat_statistics_are_pooled_and_micro_aggregated() {
        let first = beat_metrics(&[0, 500_000], &[0, 510_000]);
        let second = beat_metrics(&[0, 500_000, 1_000_000], &[20_000, 530_000, 1_040_000]);
        let aggregate = aggregate_groups(
            [
                GroupMetrics {
                    tracks: 1,
                    beat: first,
                    ..GroupMetrics::default()
                },
                GroupMetrics {
                    tracks: 1,
                    beat: second,
                    ..GroupMetrics::default()
                },
            ]
            .iter(),
        );
        assert_eq!(aggregate.beat.matched, 5);
        assert_eq!(aggregate.beat.mae_ms, Some(20.0));
        assert_eq!(aggregate.beat.p50_ms, Some(20.0));
        assert_eq!(aggregate.beat.p95_ms, Some(40.0));
        assert_eq!(aggregate.beat.precision_at_tolerance["20ms"], 3.0 / 5.0);
        assert_eq!(aggregate.beat.recall_at_tolerance["20ms"], 3.0 / 5.0);
    }

    #[test]
    fn tempo_top_n_excludes_variable_truth_and_reports_alternates() {
        let constant = GroupMetrics {
            tracks: 1,
            tempo: tempo_metrics(
                Some(&TempoTruth {
                    primary_bpm: 128.0,
                    valid_alternates_bpm: vec![64.0],
                }),
                &[NormalizedTempoHypothesis {
                    bpm: 64.0,
                    relative_weight: 1.0,
                    relation: "primary".into(),
                }],
            ),
            ..GroupMetrics::default()
        };
        let variable = GroupMetrics {
            tracks: 1,
            ..GroupMetrics::default()
        };
        let aggregate = aggregate_groups([constant.clone(), variable].iter());
        assert_eq!(aggregate.tempo.scored_tracks, 1);
        assert_eq!(aggregate.tempo.top_n_scored_tracks["1"], 1);
        assert_eq!(aggregate.tempo.correct_hypothesis_top_n_rate["1"], 0.0);
        assert_eq!(constant.tempo.canonical_hypothesis_present, Some(false));
        assert_eq!(
            constant.tempo.musically_valid_hypothesis_present,
            Some(true)
        );
    }

    #[test]
    fn transform_matching_does_not_index_shift_on_extra_or_missing_beats() {
        let base = normalized_prediction(&[0, 500_000, 1_000_000]);
        let extra = normalized_prediction(&[0, 250_000, 500_000, 1_000_000]);
        let missing = normalized_prediction(&[0, 1_000_000]);
        let mut specs = Vec::new();
        let mut normalized = BTreeMap::new();
        for (id, transform, prediction) in [
            ("base", TransformKind::None, base.clone()),
            ("extra", TransformKind::Gain { factor: 1.0 }, extra),
            ("missing", TransformKind::Compression, missing),
        ] {
            let mut fixture_spec = spec(
                id,
                FixtureFamily::Transform,
                TempoProfile::Constant { bpm: 120.0 },
                EventStyle::Standard,
                4,
                0,
                1,
            );
            fixture_spec.transform = transform;
            fixture_spec.base_id = (id != "base").then(|| "base".into());
            specs.push(FixtureRecord {
                spec: fixture_spec,
                truth: truth_with_meter(Some(4)),
                audio_sha256: "a".repeat(64),
            });
            normalized.insert(id.into(), prediction);
        }
        let metrics = transform_metrics(&specs, &normalized);
        assert_eq!(metrics["gain"].matched_beats, 3);
        assert_eq!(metrics["gain"].extra_beats, 1);
        assert_eq!(metrics["gain"].mean_beat_displacement_ms, Some(0.0));
        assert_eq!(metrics["compression"].matched_beats, 2);
        assert_eq!(metrics["compression"].missing_beats, 1);
        assert_eq!(metrics["compression"].mean_beat_displacement_ms, Some(0.0));
    }

    #[test]
    fn sample_rate_transform_preserves_duration_and_truth_timing() {
        let mut transformed = spec(
            "sample-rate",
            FixtureFamily::Transform,
            TempoProfile::Constant { bpm: 120.0 },
            EventStyle::Standard,
            4,
            0,
            1,
        );
        transformed.transform = TransformKind::SampleRate {
            sample_rate: 16_000,
        };
        transformed.sample_rate = 16_000;
        let base = generate_fixture(&FixtureSpec {
            transform: TransformKind::None,
            id: "base".into(),
            ..transformed.clone()
        })
        .unwrap();
        let converted = generate_fixture(&transformed).unwrap();
        let base_duration = base.audio.len() as f64 / f64::from(base.spec.sample_rate);
        let converted_duration =
            converted.audio.len() as f64 / f64::from(converted.spec.sample_rate);
        assert!((base_duration - converted_duration).abs() < 1.0 / 16_000.0);
        assert_eq!(
            base.truth.beat_times_micros,
            converted.truth.beat_times_micros
        );
    }

    #[test]
    fn confidence_calibration_counts_high_confidence_unmatched_predictions() {
        let truth = truth_with_meter(Some(4));
        let mut prediction = normalized_prediction(&[3_000_000]);
        prediction.beats[0].timing_confidence = 0.9;
        let metrics = metrics_for(&truth, &prediction);
        let track = TrackEvaluation {
            sample_id: "sample".into(),
            family: "test".into(),
            audio_sha256: "a".repeat(64),
            truth,
            wotoha: prediction,
            metrics,
            failure_clusters: Vec::new(),
            human_review: None,
        };
        let bin = &calibration(&[track])[4];
        assert_eq!(bin.matched_count, 0);
        assert_eq!(bin.unmatched_predicted_count, 1);
        assert_eq!(bin.match_rate, Some(0.0));
    }

    #[test]
    fn adversarial_matcher_is_monotonic_deterministic_and_minimizes_error() {
        let pairs = match_errors(
            &[0, 500_000, 1_000_000],
            &[490_000, 510_000, 1_000_000],
            Duration::from_millis(20),
        );
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].0, 1);
        assert_eq!(pairs[0].1, 0);
        assert_eq!(pairs[0].2, Duration::from_millis(10));
        assert_eq!(pairs[1].0, 2);
        assert_eq!(pairs[1].1, 2);
        assert!(
            pairs
                .windows(2)
                .all(|window| window[0].0 < window[1].0 && window[0].1 < window[1].1)
        );
    }
}
