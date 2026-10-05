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

mod classical_tempo_research;
pub use classical_tempo_research::{ClassicalTempoResearchReport, run_classical_tempo_research};
mod tempo_ambiguity_research;
use sha2::{Digest, Sha256};
pub use tempo_ambiguity_research::{TempoAmbiguityResearchReport, run_tempo_ambiguity_research};
mod tempo_shadow_followup;
pub use tempo_shadow_followup::{TempoShadowFollowupReport, run_tempo_shadow_followup};
mod tempo_conservative_shadow;
pub use tempo_conservative_shadow::{
    RealisticSyntheticCorpusReport, TempoConservativeShadowReport,
    run_independent_positive_corpus_research, run_realistic_corpus_research,
    run_tempo_conservative_shadow_research,
};
use wotoha_core::{
    analysis::{AnalysisMethod, PhraseBoundarySource, TempoRelation, TrackAnalysisV2},
    audio_analysis::LowBandFilter,
};

pub const LAB_SCHEMA_VERSION: u32 = 2;
pub const OBSERVATION_SCHEMA_VERSION: u32 = 2;
pub const REPORT_SCHEMA_VERSION: u32 = 6;
pub const RESEARCH_REPORT_SCHEMA_VERSION: u32 = 2;
pub const BLACKBOX_SCHEMA_VERSION: u32 = 2;
const DEFAULT_SEED: u64 = 0x57_4f_54_4f_48_41;
const DEFAULT_DURATION_MICROS: u64 = 12_000_000;
const DEFAULT_SAMPLE_RATE: u32 = 22_050;
const MAX_FIXTURES: usize = 256;
// The default corpus remains 12 seconds, but the lab also owns bounded
// long-duration research fixtures.  This limit is intentionally below the
// runtime model's much larger input bound and has no effect on production
// analysis or black-box package generation.
const MAX_AUDIO_SAMPLES: usize = DEFAULT_SAMPLE_RATE as usize * 120;
const MAX_PACKAGE_ENTRIES: usize = MAX_FIXTURES + 8;
const MAX_PACKAGE_FILE_BYTES: usize = 40 * 1024 * 1024;
const MAX_PACKAGE_UNCOMPRESSED_BYTES: usize = 80 * 1024 * 1024;
const BEAT_MATCH_WINDOW: Duration = Duration::from_millis(120);
// A global phase is meaningful only when the observed grid is effectively
// periodic. Three percent is deliberately broad enough to tolerate sample
// rounding and a modest isolated timing outlier, while rejecting visible
// tempo drift in this evaluation layer.
const EXTERNAL_PERIOD_STABILITY: f64 = 0.03;

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
                "artifact SHA-256 mismatch for {sample_id}: expected {expected}, actual {actual}"
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
pub struct BlackboxManifest {
    pub schema_version: u32,
    pub corpus_schema_version: u32,
    pub split: String,
    pub seed: u64,
    pub fixtures: Vec<BlackboxFixture>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BlackboxFixture {
    pub sample_id: String,
    pub audio_file: String,
    /// SHA-256 of the exact transferred RIFF/WAVE file.
    #[serde(rename = "wav_file_sha256", alias = "audio_sha256")]
    pub audio_sha256: String,
    pub pcm_sha256: String,
    /// SHA-256 of the pre-quantization generated f32 fixture, never the
    /// identity used by an external observer.
    #[serde(
        rename = "generated_float_fixture_sha256",
        alias = "fixture_audio_sha256"
    )]
    pub fixture_audio_sha256: String,
    pub sample_count: usize,
    pub sample_rate: u32,
    pub channels: u8,
    pub duration_micros: u64,
    pub ground_truth_sha256: String,
    pub spec: FixtureSpec,
    pub truth: AnalysisGroundTruth,
}

impl BlackboxManifest {
    pub fn load(path: &Path) -> Result<Self, LabError> {
        let manifest: Self = serde_json::from_slice(&fs::read(path)?)?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn validate(&self) -> Result<(), LabError> {
        if self.schema_version != BLACKBOX_SCHEMA_VERSION
            || self.corpus_schema_version != LAB_SCHEMA_VERSION
            || self.split.trim().is_empty()
            || self.fixtures.is_empty()
            || self.fixtures.len() > MAX_FIXTURES
        {
            return Err(LabError::InvalidInput(
                "invalid black-box manifest schema or bounded fixture list".into(),
            ));
        }
        let mut ids = BTreeSet::new();
        for fixture in &self.fixtures {
            if !ids.insert(fixture.sample_id.clone())
                || fixture.sample_id != fixture.spec.id
                || !is_safe_relative_path(&fixture.audio_file)
            {
                return Err(LabError::InvalidInput(format!(
                    "invalid or duplicate black-box fixture {}",
                    fixture.sample_id
                )));
            }
            fixture.spec.validate()?;
            validate_sha256(&fixture.audio_sha256, "audio")?;
            validate_sha256(&fixture.pcm_sha256, "PCM")?;
            validate_sha256(&fixture.fixture_audio_sha256, "fixture")?;
            validate_sha256(&fixture.ground_truth_sha256, "ground truth")?;
            if fixture.sample_count == 0
                || fixture.sample_rate == 0
                || fixture.channels == 0
                || fixture.duration_micros != fixture.truth.duration_micros
            {
                return Err(LabError::InvalidInput(format!(
                    "invalid black-box metadata for {}",
                    fixture.sample_id
                )));
            }
        }
        Ok(())
    }
}

fn validate_sha256(value: &str, label: &str) -> Result<(), LabError> {
    if value.len() == 64 && value.chars().all(|character| character.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(LabError::InvalidInput(format!("invalid {label} SHA-256")))
    }
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
        self.validate_structure()?;
        for fixture in &self.fixtures {
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

    fn validate_structure(&self) -> Result<(), LabError> {
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
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FixtureRecord {
    pub spec: FixtureSpec,
    pub truth: AnalysisGroundTruth,
    /// SHA-256 of generated, pre-quantization f32 samples. This is only an
    /// internal corpus identity; exported WAV identity is separate.
    #[serde(rename = "generated_float_fixture_sha256")]
    pub audio_sha256: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct FixtureSpec {
    pub id: String,
    pub family: FixtureFamily,
    pub duration_micros: u64,
    pub sample_rate: u32,
    pub channels: u8,
    pub lead_in_micros: u64,
    pub meter: u8,
    /// Evaluation truth is explicit and may intentionally be unknown even
    /// when synthesis uses a valid meter for pulse placement.
    pub meter_truth: Option<u8>,
    pub tempo: TempoProfile,
    pub event_style: EventStyle,
    pub transform: TransformKind,
    pub base_id: Option<String>,
    pub seed: u64,
}

impl<'de> Deserialize<'de> for FixtureSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            id: String,
            family: FixtureFamily,
            duration_micros: u64,
            sample_rate: u32,
            channels: u8,
            lead_in_micros: u64,
            meter: u8,
            // `Option<Option<T>>` preserves the distinction between an
            // omitted field and an explicit JSON null.
            #[serde(default, deserialize_with = "deserialize_nullable_presence")]
            meter_truth: Option<Option<u8>>,
            tempo: TempoProfile,
            event_style: EventStyle,
            transform: TransformKind,
            base_id: Option<String>,
            seed: u64,
        }

        let wire = Wire::deserialize(deserializer)?;
        let meter_truth = wire.meter_truth.ok_or_else(|| {
            <D::Error as serde::de::Error>::custom(
                "schema-v2 fixture spec requires an explicit meter_truth field",
            )
        })?;
        Ok(Self {
            id: wire.id,
            family: wire.family,
            duration_micros: wire.duration_micros,
            sample_rate: wire.sample_rate,
            channels: wire.channels,
            lead_in_micros: wire.lead_in_micros,
            meter: wire.meter,
            meter_truth,
            tempo: wire.tempo,
            event_style: wire.event_style,
            transform: wire.transform,
            base_id: wire.base_id,
            seed: wire.seed,
        })
    }
}

fn deserialize_nullable_presence<'de, D>(deserializer: D) -> Result<Option<Option<u8>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Some(Option::<u8>::deserialize(deserializer)?))
}

impl FixtureSpec {
    fn validate(&self) -> Result<(), LabError> {
        if self.id.trim().is_empty()
            || self.duration_micros == 0
            || self.duration_micros > 120_000_000
            || self.sample_rate == 0
            || self.channels == 0
            || !matches!(self.meter, 2 | 3 | 4 | 6)
            || self
                .meter_truth
                .is_some_and(|meter| !matches!(meter, 2 | 3 | 4 | 6))
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
    MeterClear,
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

    fn expectation(&self) -> TransformExpectation {
        match self {
            Self::HighPass => TransformExpectation::EvidenceAblation,
            _ => TransformExpectation::Invariant,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransformExpectation {
    #[default]
    Invariant,
    EvidenceAblation,
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
    for (id, bpm, style) in [
        (
            "half-double-64-128",
            128.0,
            EventStyle::StrongEverySecondBeat,
        ),
        ("half-double-70-140", 140.0, EventStyle::WeakSubdivision),
        ("half-double-75-150", 150.0, EventStyle::HatsOnly),
        (
            "half-double-80-160",
            160.0,
            EventStyle::StrongEverySecondBeat,
        ),
        ("half-double-85-170", 170.0, EventStyle::WeakSubdivision),
        ("half-double-90-180", 180.0, EventStyle::HatsOnly),
    ] {
        specs.push(spec(
            id,
            FixtureFamily::HalfDouble,
            TempoProfile::Constant { bpm },
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
        (2, "meter-clear-2-4"),
        (3, "meter-clear-3-4"),
        (4, "meter-clear-4-4"),
        (6, "meter-clear-6-8"),
    ] {
        specs.push(spec(
            id,
            FixtureFamily::Meter,
            TempoProfile::Constant { bpm: 120.0 },
            EventStyle::MeterClear,
            meter,
            450_000,
            seed,
        ));
    }
    let mut ambiguous_meter = spec(
        "meter-ambiguous-4-4",
        FixtureFamily::Meter,
        TempoProfile::Constant { bpm: 120.0 },
        EventStyle::Standard,
        4,
        450_000,
        seed,
    );
    ambiguous_meter.meter_truth = None;
    specs.push(ambiguous_meter);
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
        meter_truth: Some(meter),
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

#[derive(Clone, Debug)]
struct DecodedWav {
    samples: Vec<f32>,
    pcm: Vec<i16>,
    sample_rate: u32,
    channels: u8,
    frame_count: usize,
    pcm_sha256: String,
}

fn quantize_pcm16(samples: &[f32]) -> Vec<i16> {
    samples
        .iter()
        .map(|sample| (sample.clamp(-1.0, 1.0) * 32_767.0).round() as i16)
        .collect()
}

fn pcm16_bytes(samples: &[i16]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(samples.len() * 2);
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    bytes
}

fn pcm_sha256(samples: &[i16]) -> String {
    hash_bytes(&pcm16_bytes(samples))
}

fn hash_bytes(bytes: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(bytes);
    format!("{:x}", digest.finalize())
}

fn encode_wav_pcm16(samples: &[f32], sample_rate: u32, channels: u8) -> Vec<u8> {
    let pcm = pcm16_bytes(&quantize_pcm16(samples));
    let byte_rate = sample_rate
        .saturating_mul(u32::from(channels))
        .saturating_mul(2);
    let block_align = u16::from(channels).saturating_mul(2);
    let riff_size = 36_u32.saturating_add(pcm.len() as u32);
    let mut wav = Vec::with_capacity(44 + pcm.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&riff_size.to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&u16::from(channels).to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&block_align.to_le_bytes());
    wav.extend_from_slice(&16_u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
    wav.extend_from_slice(&pcm);
    wav
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes([
        *bytes.get(offset)?,
        *bytes.get(offset + 1)?,
    ]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes([
        *bytes.get(offset)?,
        *bytes.get(offset + 1)?,
        *bytes.get(offset + 2)?,
        *bytes.get(offset + 3)?,
    ]))
}

fn decode_wav_pcm16(bytes: &[u8]) -> Result<DecodedWav, LabError> {
    if bytes.len() < 44 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(LabError::InvalidInput("WAV must be RIFF/WAVE".into()));
    }
    let mut offset = 12;
    let mut channels = None;
    let mut sample_rate = None;
    let mut bits_per_sample = None;
    let mut pcm = None;
    while offset + 8 <= bytes.len() {
        let id = &bytes[offset..offset + 4];
        let size = read_u32(bytes, offset + 4)
            .ok_or_else(|| LabError::InvalidInput("truncated WAV chunk".into()))?
            as usize;
        let start = offset + 8;
        let end = start
            .checked_add(size)
            .ok_or_else(|| LabError::InvalidInput("WAV chunk is too large".into()))?;
        if end > bytes.len() {
            return Err(LabError::InvalidInput("WAV chunk exceeds file".into()));
        }
        match id {
            b"fmt " if size >= 16 => {
                if read_u16(bytes, start) != Some(1) {
                    return Err(LabError::InvalidInput(
                        "WAV must use uncompressed PCM".into(),
                    ));
                }
                channels = read_u16(bytes, start + 2).map(|value| value as u8);
                sample_rate = read_u32(bytes, start + 4);
                bits_per_sample = read_u16(bytes, start + 14);
            }
            b"data" => pcm = Some(&bytes[start..end]),
            _ => {}
        }
        offset = end + (size % 2);
    }
    let channels = channels
        .filter(|channels| *channels > 0)
        .ok_or_else(|| LabError::InvalidInput("WAV fmt chunk is missing".into()))?;
    let sample_rate = sample_rate
        .filter(|sample_rate| *sample_rate > 0)
        .ok_or_else(|| LabError::InvalidInput("WAV sample rate is missing".into()))?;
    if bits_per_sample != Some(16) {
        return Err(LabError::InvalidInput(
            "WAV must use signed 16-bit PCM".into(),
        ));
    }
    let pcm = pcm.ok_or_else(|| LabError::InvalidInput("WAV data chunk is missing".into()))?;
    if pcm.len() % 2 != 0 {
        return Err(LabError::InvalidInput(
            "WAV PCM payload is misaligned".into(),
        ));
    }
    let mut integer_samples = Vec::with_capacity(pcm.len() / 2);
    for bytes in pcm.chunks_exact(2) {
        integer_samples.push(i16::from_le_bytes([bytes[0], bytes[1]]));
    }
    let channel_count = usize::from(channels);
    if integer_samples.len() % channel_count != 0 {
        return Err(LabError::InvalidInput(
            "WAV payload does not contain complete frames".into(),
        ));
    }
    let samples = integer_samples
        .iter()
        .map(|sample| *sample as f32 / 32_768.0)
        .collect::<Vec<_>>();
    let pcm_sha256 = pcm_sha256(&integer_samples);
    Ok(DecodedWav {
        frame_count: integer_samples.len() / channel_count,
        samples,
        pcm: integer_samples,
        sample_rate,
        channels,
        pcm_sha256,
    })
}

fn ground_truth_sha256(truth: &AnalysisGroundTruth) -> Result<String, LabError> {
    Ok(hash_bytes(&serde_json::to_vec(truth)?))
}

pub fn export_blackbox(output: &Path, seed: u64) -> Result<BlackboxManifest, LabError> {
    if output.exists() {
        if !output.is_dir() {
            return Err(LabError::InvalidInput(format!(
                "black-box output is not a directory: {}",
                output.display()
            )));
        }
    } else {
        fs::create_dir_all(output)?;
    }
    let audio_root = output.join("audio");
    fs::create_dir_all(&audio_root)?;
    let source = generate_default_manifest(seed)?;
    let mut fixtures = Vec::with_capacity(source.fixtures.len());
    for record in source.fixtures {
        let fixture = generate_fixture(&record.spec)?;
        let wav = encode_wav_pcm16(
            &fixture.audio,
            fixture.spec.sample_rate,
            fixture.spec.channels,
        );
        let decoded = decode_wav_pcm16(&wav)?;
        let expected_pcm = quantize_pcm16(&fixture.audio);
        if decoded.sample_rate != fixture.spec.sample_rate
            || decoded.channels != fixture.spec.channels
            || decoded.frame_count != fixture.audio.len() / usize::from(fixture.spec.channels)
            || expected_pcm != decoded.pcm
        {
            return Err(LabError::InvalidInput(format!(
                "WAV round-trip mismatch for {}",
                fixture.spec.id
            )));
        }
        let relative_audio = format!("audio/{}.wav", fixture.spec.id);
        fs::write(output.join(&relative_audio), &wav)?;
        fixtures.push(BlackboxFixture {
            sample_id: fixture.spec.id.clone(),
            audio_file: relative_audio,
            audio_sha256: hash_bytes(&wav),
            pcm_sha256: decoded.pcm_sha256,
            fixture_audio_sha256: fixture.audio_sha256,
            sample_count: decoded.samples.len(),
            sample_rate: decoded.sample_rate,
            channels: decoded.channels,
            duration_micros: fixture.truth.duration_micros,
            ground_truth_sha256: ground_truth_sha256(&fixture.truth)?,
            spec: fixture.spec,
            truth: fixture.truth,
        });
    }
    let manifest = BlackboxManifest {
        schema_version: BLACKBOX_SCHEMA_VERSION,
        corpus_schema_version: LAB_SCHEMA_VERSION,
        split: source.split,
        seed: source.seed,
        fixtures,
    };
    manifest.validate()?;
    write_json(&output.join("manifest.json"), &manifest)?;
    let observations = manifest
        .fixtures
        .iter()
        .map(|fixture| ExternalAnalysisObservation {
            schema_version: OBSERVATION_SCHEMA_VERSION,
            sample_id: fixture.sample_id.clone(),
            audio_file: Some(fixture.audio_file.clone()),
            audio_sha256: fixture.audio_sha256.clone(),
            pcm_sha256: Some(fixture.pcm_sha256.clone()),
            ground_truth_sha256: Some(fixture.ground_truth_sha256.clone()),
            observer: ObserverIdentity {
                product: "external-observer".into(),
                version: "fill-me".into(),
                platform: Some("fill-me".into()),
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
            timing: ObservationTiming {
                analysis_elapsed_millis: None,
                observed_duration_micros: Some(fixture.duration_micros),
            },
            notes: vec![
                "Import the WAV and run normal public analysis without manual correction.".into(),
                "Record only public observations; disagreements are not automatically errors."
                    .into(),
            ],
        })
        .collect();
    write_json(
        &output.join("external-observations-template.json"),
        &ExternalObservationDocument {
            schema_version: OBSERVATION_SCHEMA_VERSION,
            observations,
        },
    )?;
    let mut checksum_lines = Vec::new();
    for fixture in &manifest.fixtures {
        let bytes = fs::read(output.join(&fixture.audio_file))?;
        checksum_lines.push(format!("{}  {}", hash_bytes(&bytes), fixture.audio_file));
    }
    let manifest_bytes = fs::read(output.join("manifest.json"))?;
    checksum_lines.push(format!("{}  manifest.json", hash_bytes(&manifest_bytes)));
    let template_bytes = fs::read(output.join("external-observations-template.json"))?;
    checksum_lines.push(format!(
        "{}  external-observations-template.json",
        hash_bytes(&template_bytes)
    ));
    checksum_lines.sort_unstable();
    fs::write(
        output.join("checksums.sha256"),
        checksum_lines.join("\n") + "\n",
    )?;
    fs::write(
        output.join("README.md"),
        blackbox_readme(manifest.fixtures.len(), manifest.seed),
    )?;
    Ok(manifest)
}

/// Verify a generated black-box directory before it is handed to another
/// analysis tool. This intentionally checks both the manifest's semantic
/// hashes and the package-level checksum file.
pub fn verify_blackbox_directory(input: &Path) -> Result<(), LabError> {
    if !input.is_dir() {
        return Err(LabError::InvalidInput(format!(
            "black-box input is not a directory: {}",
            input.display()
        )));
    }
    let manifest_path = input.join("manifest.json");
    let manifest = BlackboxManifest::load(&manifest_path)?;
    let template_path = input.join("external-observations-template.json");
    let template: ExternalObservationDocument = serde_json::from_slice(&fs::read(&template_path)?)?;
    template.validate()?;
    let expected_ids = manifest
        .fixtures
        .iter()
        .map(|fixture| fixture.sample_id.as_str())
        .collect::<BTreeSet<_>>();
    if template.observations.len() != expected_ids.len()
        || template
            .observations
            .iter()
            .map(|observation| observation.sample_id.as_str())
            .collect::<BTreeSet<_>>()
            != expected_ids
    {
        return Err(LabError::InvalidInput(
            "external observation template identities do not match manifest".into(),
        ));
    }
    for fixture in &manifest.fixtures {
        let path = input.join(&fixture.audio_file);
        if !is_safe_relative_path(&fixture.audio_file) {
            return Err(LabError::InvalidInput(format!(
                "unsafe black-box path {}",
                fixture.audio_file
            )));
        }
        let bytes = fs::read(&path)?;
        if bytes.len() > MAX_PACKAGE_FILE_BYTES || hash_bytes(&bytes) != fixture.audio_sha256 {
            return Err(LabError::HashMismatch {
                sample_id: fixture.sample_id.clone(),
                expected: fixture.audio_sha256.clone(),
                actual: hash_bytes(&bytes),
            });
        }
        let decoded = decode_wav_pcm16(&bytes)?;
        if decoded.pcm_sha256 != fixture.pcm_sha256
            || decoded.sample_rate != fixture.sample_rate
            || decoded.channels != fixture.channels
            || decoded.samples.len() != fixture.sample_count
        {
            return Err(LabError::InvalidInput(format!(
                "WAV or PCM verification failed for {}",
                fixture.sample_id
            )));
        }
        if ground_truth_sha256(&fixture.truth)? != fixture.ground_truth_sha256 {
            return Err(LabError::HashMismatch {
                sample_id: fixture.sample_id.clone(),
                expected: fixture.ground_truth_sha256.clone(),
                actual: ground_truth_sha256(&fixture.truth)?,
            });
        }
        let observation = template
            .observations
            .iter()
            .find(|observation| observation.sample_id == fixture.sample_id)
            .ok_or_else(|| LabError::UnknownSample(fixture.sample_id.clone()))?;
        if observation.audio_sha256 != fixture.audio_sha256
            || observation.pcm_sha256.as_deref() != Some(fixture.pcm_sha256.as_str())
            || observation.ground_truth_sha256.as_deref()
                != Some(fixture.ground_truth_sha256.as_str())
        {
            return Err(LabError::InvalidInput(format!(
                "observation identity mismatch for {}",
                fixture.sample_id
            )));
        }
    }
    verify_checksum_file(input, &manifest)?;
    Ok(())
}

/// Create a deterministic, stored ZIP handoff artifact. Stored entries avoid
/// compressor-version variability; stable ordering and zero DOS timestamps
/// make identical corpus directories produce identical bytes.
pub fn package_blackbox(input: &Path, output: &Path) -> Result<(), LabError> {
    verify_blackbox_directory(input)?;
    let manifest: BlackboxManifest =
        serde_json::from_slice(&fs::read(input.join("manifest.json"))?)?;
    let mut paths = manifest
        .fixtures
        .iter()
        .map(|fixture| fixture.audio_file.clone())
        .collect::<Vec<_>>();
    paths.extend([
        "README.md".into(),
        "checksums.sha256".into(),
        "external-observations-template.json".into(),
        "manifest.json".into(),
    ]);
    paths.sort();
    let mut entries = Vec::with_capacity(paths.len());
    for path in paths {
        if !is_safe_relative_path(&path) {
            return Err(LabError::InvalidInput(format!(
                "unsafe package path {path}"
            )));
        }
        let bytes = fs::read(input.join(&path))?;
        if bytes.len() > MAX_PACKAGE_FILE_BYTES {
            return Err(LabError::InvalidInput(format!(
                "package file is too large: {path}"
            )));
        }
        entries.push((path, bytes));
    }
    let mut archive = Vec::new();
    let mut central = Vec::new();
    for (path, bytes) in &entries {
        let offset = u32::try_from(archive.len())
            .map_err(|_| LabError::InvalidInput("ZIP exceeds 4 GiB offset limit".into()))?;
        let name = path.as_bytes();
        let size = u32::try_from(bytes.len())
            .map_err(|_| LabError::InvalidInput("ZIP entry exceeds 4 GiB".into()))?;
        let crc = crc32(bytes);
        write_zip_local_header(&mut archive, name, crc, size);
        archive.extend_from_slice(bytes);
        write_zip_central_header(&mut central, name, crc, size, offset);
    }
    let central_offset = u32::try_from(archive.len())
        .map_err(|_| LabError::InvalidInput("ZIP exceeds 4 GiB".into()))?;
    archive.extend_from_slice(&central);
    let central_size = u32::try_from(central.len())
        .map_err(|_| LabError::InvalidInput("ZIP central directory exceeds 4 GiB".into()))?;
    let count = u16::try_from(entries.len())
        .map_err(|_| LabError::InvalidInput("too many ZIP entries".into()))?;
    write_u32(&mut archive, 0x0605_4b50);
    write_u16(&mut archive, 0);
    write_u16(&mut archive, 0);
    write_u16(&mut archive, count);
    write_u16(&mut archive, count);
    write_u32(&mut archive, central_size);
    write_u32(&mut archive, central_offset);
    write_u16(&mut archive, 0);
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(output, archive)?;
    verify_blackbox_package(output)?;
    Ok(())
}

/// Verify a deterministic black-box ZIP with bounded stored entries, safe
/// relative paths, duplicate detection, CRCs, and all corpus identities.
pub fn verify_blackbox_package(path: &Path) -> Result<(), LabError> {
    let bytes = fs::read(path)?;
    let entries = read_stored_zip(&bytes)?;
    let total = entries.iter().map(|(_, data)| data.len()).sum::<usize>();
    if total > MAX_PACKAGE_UNCOMPRESSED_BYTES {
        return Err(LabError::InvalidInput(
            "ZIP uncompressed size is too large".into(),
        ));
    }
    let entry_names = entries
        .iter()
        .map(|(name, _)| name.clone())
        .collect::<BTreeSet<_>>();
    let manifest_bytes = entries
        .iter()
        .find(|(name, _)| name == "manifest.json")
        .map(|(_, bytes)| bytes)
        .ok_or_else(|| LabError::InvalidInput("ZIP manifest is missing".into()))?;
    let manifest: BlackboxManifest = serde_json::from_slice(manifest_bytes)?;
    manifest.validate()?;
    let mut expected_names = manifest
        .fixtures
        .iter()
        .map(|fixture| fixture.audio_file.clone())
        .collect::<BTreeSet<_>>();
    expected_names.extend([
        "README.md".into(),
        "checksums.sha256".into(),
        "external-observations-template.json".into(),
        "manifest.json".into(),
    ]);
    if entry_names != expected_names {
        return Err(LabError::InvalidInput(
            "ZIP contains unexpected or missing corpus files".into(),
        ));
    }
    let temp = std::env::temp_dir().join(format!(
        "wotoha-blackbox-verify-{}-{}",
        std::process::id(),
        hash_bytes(&bytes)[..16].to_owned()
    ));
    if temp.exists() {
        return Err(LabError::InvalidInput(
            "verification directory already exists".into(),
        ));
    }
    fs::create_dir_all(temp.join("audio"))?;
    for (name, data) in &entries {
        if !is_safe_relative_path(name) || name.is_empty() {
            return Err(LabError::InvalidInput(format!("unsafe ZIP path {name}")));
        }
        let destination = temp.join(name);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(destination, data)?;
    }
    let result = verify_blackbox_directory(&temp);
    let cleanup = fs::remove_dir_all(&temp);
    result.and(cleanup.map_err(LabError::Io))
}

fn verify_checksum_file(input: &Path, manifest: &BlackboxManifest) -> Result<(), LabError> {
    let checksum_bytes = fs::read(input.join("checksums.sha256"))?;
    let text = std::str::from_utf8(&checksum_bytes)
        .map_err(|_| LabError::InvalidInput("checksums.sha256 is not UTF-8".into()))?;
    let mut seen = BTreeSet::new();
    for line in text.lines() {
        let Some((hash, path)) = line.split_once("  ") else {
            return Err(LabError::InvalidInput("invalid checksum line".into()));
        };
        validate_sha256(hash, "checksum")?;
        if !seen.insert(path.to_owned()) || !is_safe_relative_path(path) {
            return Err(LabError::InvalidInput(
                "invalid or duplicate checksum path".into(),
            ));
        }
        let actual = hash_bytes(&fs::read(input.join(path))?);
        if actual != hash {
            return Err(LabError::HashMismatch {
                sample_id: path.into(),
                expected: hash.into(),
                actual,
            });
        }
    }
    let mut expected = manifest
        .fixtures
        .iter()
        .map(|fixture| fixture.audio_file.clone())
        .collect::<BTreeSet<_>>();
    expected.insert("manifest.json".into());
    expected.insert("external-observations-template.json".into());
    if seen != expected {
        return Err(LabError::InvalidInput(
            "checksums.sha256 does not cover exactly the corpus files".into(),
        ));
    }
    Ok(())
}

fn is_safe_relative_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path.split('/').any(|part| part.is_empty() || part == "..")
        && !path.contains(':')
}

fn write_u16(output: &mut Vec<u8>, value: u16) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn write_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn write_zip_local_header(output: &mut Vec<u8>, name: &[u8], crc: u32, size: u32) {
    write_u32(output, 0x0403_4b50);
    write_u16(output, 20);
    write_u16(output, 0);
    write_u16(output, 0);
    write_u16(output, 0);
    write_u16(output, 0);
    write_u32(output, crc);
    write_u32(output, size);
    write_u32(output, size);
    write_u16(output, name.len() as u16);
    write_u16(output, 0);
    output.extend_from_slice(name);
}

fn write_zip_central_header(output: &mut Vec<u8>, name: &[u8], crc: u32, size: u32, offset: u32) {
    write_u32(output, 0x0201_4b50);
    write_u16(output, 20);
    write_u16(output, 20);
    write_u16(output, 0);
    write_u16(output, 0);
    write_u16(output, 0);
    write_u16(output, 0);
    write_u32(output, crc);
    write_u32(output, size);
    write_u32(output, size);
    write_u16(output, name.len() as u16);
    write_u16(output, 0);
    write_u16(output, 0);
    write_u16(output, 0);
    write_u16(output, 0);
    write_u32(output, 0);
    write_u32(output, offset);
    output.extend_from_slice(name);
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn read_stored_zip(bytes: &[u8]) -> Result<Vec<(String, Vec<u8>)>, LabError> {
    if bytes.len() > MAX_PACKAGE_UNCOMPRESSED_BYTES + 1_000_000 {
        return Err(LabError::InvalidInput("ZIP input is too large".into()));
    }
    let start = bytes.len().saturating_sub(65_557);
    let eocd = bytes[start..]
        .windows(4)
        .rposition(|window| window == 0x0605_4b50u32.to_le_bytes())
        .map(|offset| start + offset)
        .ok_or_else(|| LabError::InvalidInput("ZIP end record is missing".into()))?;
    let count = usize::from(zip_read_u16(bytes, eocd + 10)?);
    let central_size = usize::try_from(zip_read_u32(bytes, eocd + 12)?)
        .map_err(|_| LabError::InvalidInput("invalid ZIP central size".into()))?;
    let central_offset = usize::try_from(zip_read_u32(bytes, eocd + 16)?)
        .map_err(|_| LabError::InvalidInput("invalid ZIP central offset".into()))?;
    if count == 0 || count > MAX_PACKAGE_ENTRIES || central_offset + central_size > bytes.len() {
        return Err(LabError::InvalidInput(
            "invalid ZIP entry count or central directory".into(),
        ));
    }
    let mut cursor = central_offset;
    let mut entries = Vec::with_capacity(count);
    let mut names = BTreeSet::new();
    for _ in 0..count {
        if zip_read_u32(bytes, cursor)? != 0x0201_4b50 {
            return Err(LabError::InvalidInput("invalid ZIP central entry".into()));
        }
        let method = zip_read_u16(bytes, cursor + 10)?;
        let compressed = usize::try_from(zip_read_u32(bytes, cursor + 20)?)
            .map_err(|_| LabError::InvalidInput("invalid ZIP compressed size".into()))?;
        let uncompressed = usize::try_from(zip_read_u32(bytes, cursor + 24)?)
            .map_err(|_| LabError::InvalidInput("invalid ZIP uncompressed size".into()))?;
        let name_len = usize::from(zip_read_u16(bytes, cursor + 28)?);
        let extra_len = usize::from(zip_read_u16(bytes, cursor + 30)?);
        let comment_len = usize::from(zip_read_u16(bytes, cursor + 32)?);
        let local_offset = usize::try_from(zip_read_u32(bytes, cursor + 42)?)
            .map_err(|_| LabError::InvalidInput("invalid ZIP local offset".into()))?;
        let name_start = cursor + 46;
        let name_end = name_start + name_len;
        if method != 0
            || compressed != uncompressed
            || uncompressed > MAX_PACKAGE_FILE_BYTES
            || name_end + extra_len + comment_len > bytes.len()
        {
            return Err(LabError::InvalidInput(
                "unsupported or oversized ZIP entry".into(),
            ));
        }
        let name = std::str::from_utf8(&bytes[name_start..name_end])
            .map_err(|_| LabError::InvalidInput("ZIP path is not UTF-8".into()))?
            .to_owned();
        if !is_safe_relative_path(&name) || !names.insert(name.clone()) {
            return Err(LabError::InvalidInput(
                "unsafe or duplicate ZIP path".into(),
            ));
        }
        if zip_read_u32(bytes, local_offset)? != 0x0403_4b50 {
            return Err(LabError::InvalidInput("invalid ZIP local entry".into()));
        }
        let local_name_len = usize::from(zip_read_u16(bytes, local_offset + 26)?);
        let local_extra_len = usize::from(zip_read_u16(bytes, local_offset + 28)?);
        let data_start = local_offset + 30 + local_name_len + local_extra_len;
        let data_end = data_start + uncompressed;
        if data_end > bytes.len()
            || bytes[local_offset + 30..data_start].get(..local_name_len) != Some(name.as_bytes())
        {
            return Err(LabError::InvalidInput(
                "invalid ZIP local path or bounds".into(),
            ));
        }
        let data = bytes[data_start..data_end].to_vec();
        if crc32(&data) != zip_read_u32(bytes, cursor + 16)? {
            return Err(LabError::InvalidInput(format!(
                "ZIP CRC mismatch for {name}"
            )));
        }
        entries.push((name, data));
        cursor = name_end + extra_len + comment_len;
    }
    if cursor != central_offset + central_size {
        return Err(LabError::InvalidInput(
            "ZIP central directory size mismatch".into(),
        ));
    }
    Ok(entries)
}

fn zip_read_u16(bytes: &[u8], offset: usize) -> Result<u16, LabError> {
    let values = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| LabError::InvalidInput("truncated ZIP".into()))?;
    Ok(u16::from_le_bytes([values[0], values[1]]))
}

fn zip_read_u32(bytes: &[u8], offset: usize) -> Result<u32, LabError> {
    let values = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| LabError::InvalidInput("truncated ZIP".into()))?;
    Ok(u32::from_le_bytes([
        values[0], values[1], values[2], values[3],
    ]))
}

pub fn evaluate_exported_manifest(
    manifest_path: &Path,
    audio_root: &Path,
    external: Option<&ExternalObservationDocument>,
    options: EvaluationOptions,
) -> Result<EvaluationReport, LabError> {
    let blackbox = BlackboxManifest::load(manifest_path)?;
    let mut records = Vec::with_capacity(blackbox.fixtures.len());
    let mut audio_overrides = BTreeMap::new();
    let external_identities = blackbox
        .fixtures
        .iter()
        .map(|fixture| {
            (
                fixture.sample_id.clone(),
                ExternalIdentity {
                    wav_file_sha256: fixture.audio_sha256.clone(),
                    pcm_sha256: fixture.pcm_sha256.clone(),
                    ground_truth_sha256: fixture.ground_truth_sha256.clone(),
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    for fixture in &blackbox.fixtures {
        let path = audio_root.join(&fixture.audio_file);
        let bytes = fs::read(&path)?;
        let actual_file_sha256 = hash_bytes(&bytes);
        if actual_file_sha256 != fixture.audio_sha256 {
            return Err(LabError::HashMismatch {
                sample_id: fixture.sample_id.clone(),
                expected: fixture.audio_sha256.clone(),
                actual: actual_file_sha256,
            });
        }
        let decoded = decode_wav_pcm16(&bytes)?;
        if decoded.pcm_sha256 != fixture.pcm_sha256
            || decoded.samples.len() != fixture.sample_count
            || decoded.sample_rate != fixture.sample_rate
            || decoded.channels != fixture.channels
        {
            return Err(LabError::InvalidInput(format!(
                "exported WAV metadata or PCM hash mismatch for {}",
                fixture.sample_id
            )));
        }
        if ground_truth_sha256(&fixture.truth)? != fixture.ground_truth_sha256 {
            return Err(LabError::HashMismatch {
                sample_id: fixture.sample_id.clone(),
                expected: fixture.ground_truth_sha256.clone(),
                actual: ground_truth_sha256(&fixture.truth)?,
            });
        }
        records.push(FixtureRecord {
            spec: fixture.spec.clone(),
            truth: fixture.truth.clone(),
            audio_sha256: fixture.audio_sha256.clone(),
        });
        audio_overrides.insert(fixture.sample_id.clone(), decoded.samples);
    }
    let manifest = SyntheticCorpusManifest {
        schema_version: LAB_SCHEMA_VERSION,
        split: blackbox.split,
        seed: blackbox.seed,
        fixtures: records,
    };
    evaluate_manifest_with_context(
        &manifest,
        external,
        options,
        &AtomicBool::new(false),
        Some(&audio_overrides),
        Some(&external_identities),
    )
}

fn blackbox_readme(fixture_count: usize, seed: u64) -> String {
    format!(
        "# Wotoha clean-room black-box corpus\n\nThis package contains {fixture_count} deterministic synthetic WAV fixtures generated with seed `{seed}`. The WAV PCM16 bytes are the canonical artifacts for external analysis. The manifest labels the WAV file SHA-256, decoded PCM SHA-256, generated fixture SHA-256, and Ground Truth SHA-256 separately.\n\n## Team A workflow\n\n1. Import each WAV into the public application.\n2. Trigger normal public track analysis.\n3. Do not manually correct the result before recording it.\n4. Record reported BPM, beatgrid, visible downbeat/grid phase, and musical key where available.\n5. Preserve public analysis settings and analysis timing.\n6. Fill `external-observations-template.json`; keep identity hashes unchanged.\n7. Record external analysis output before inspecting Ground Truth.\n8. Do not interpret disagreement as error without comparing against Ground Truth.\n\nExternal software output is reference observation, never Ground Truth. This corpus and schema contain no proprietary implementation information.\n"
    )
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
    let meter = spec.meter_truth;
    AnalysisGroundTruth {
        tempo,
        meter,
        beat_times_micros: beats,
        downbeats,
        tempo_segments,
        duration_micros: spec.duration_micros,
    }
}

fn meter_accent(meter: u8, beat: usize) -> (f32, f32, f32) {
    match meter {
        2 => match beat {
            0 => (0.95, 0.85, 0.75),
            _ => (0.30, 0.25, 0.45),
        },
        3 => match beat {
            0 => (0.95, 0.85, 0.75),
            _ => (0.30, 0.25, 0.45),
        },
        4 => match beat {
            0 => (0.95, 0.85, 0.75),
            2 => (0.62, 0.55, 0.60),
            _ => (0.28, 0.24, 0.42),
        },
        6 => match beat {
            0 => (0.95, 0.85, 0.75),
            3 => (0.62, 0.55, 0.60),
            _ => (0.28, 0.24, 0.42),
        },
        _ => (0.0, 0.0, 0.0),
    }
}

fn synthesize_audio(spec: &FixtureSpec, truth: &AnalysisGroundTruth) -> Vec<f32> {
    let sample_count =
        (spec.duration_micros as f64 * spec.sample_rate as f64 / 1_000_000.0).round() as usize;
    let mut audio = vec![0.0; sample_count];
    let mut rng = Lcg::new(spec.seed);
    for (index, &beat_micros) in truth.beat_times_micros.iter().enumerate() {
        let beat = index % spec.meter as usize;
        let meter_clear = spec.event_style == EventStyle::MeterClear;
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
        let kick_gain = if meter_clear {
            meter_accent(spec.meter, beat).0
        } else {
            match spec.event_style {
                EventStyle::AttenuatedKick => 0.20,
                EventStyle::KickOnly | EventStyle::ClearDownbeat => 0.90,
                _ => 0.65,
            }
        };
        if allow_kick
            && !matches!(
                spec.event_style,
                EventStyle::SnareOnly | EventStyle::HatsOnly
            )
        {
            add_burst(&mut audio, spec.sample_rate, beat_micros, 55.0, kick_gain);
        }
        if meter_clear {
            let (_, mid_gain, high_gain) = meter_accent(spec.meter, beat);
            add_burst(&mut audio, spec.sample_rate, beat_micros, 220.0, mid_gain);
            add_burst(
                &mut audio,
                spec.sample_rate,
                beat_micros,
                3_600.0,
                high_gain,
            );
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
    #[serde(default)]
    pub audio_file: Option<String>,
    /// SHA-256 of the actual transferred WAV artifact observed externally.
    #[serde(rename = "wav_file_sha256")]
    pub audio_sha256: String,
    #[serde(default)]
    pub pcm_sha256: Option<String>,
    #[serde(default)]
    pub ground_truth_sha256: Option<String>,
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
                || observation.audio_file.is_none()
                || !observation
                    .audio_file
                    .as_deref()
                    .is_some_and(is_safe_relative_path)
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
            if let Some(pcm_sha256) = observation.pcm_sha256.as_deref() {
                validate_sha256(pcm_sha256, "external PCM")?;
            }
            if let Some(ground_truth_sha256) = observation.ground_truth_sha256.as_deref() {
                validate_sha256(ground_truth_sha256, "external ground truth")?;
            }
            let settings_key = observation_settings_key(&observation.analysis_settings);
            let record_key = format!(
                "{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}",
                observation.sample_id,
                observation.observer.product,
                observation.observer.version,
                observation.observer.platform.as_deref().unwrap_or(""),
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
    analyze_fixture_with_backend(fixture, mode).map(|(analysis, _, _, _)| analysis)
}

fn normalize_neural_diagnostics(
    diagnostics: &wotoha_core::beat_analysis::NeuralRhythmDiagnostics,
) -> NeuralDiagnostics {
    NeuralDiagnostics {
        selected_period_frames: diagnostics.selected_period_frames,
        selected_bpm: diagnostics.selected_bpm,
        path_marker_count: diagnostics.path_marker_count,
        path_coverage: diagnostics.path_coverage,
        activation_mean: diagnostics.activation_mean,
        support: diagnostics.support,
        interval_residual: diagnostics.interval_residual,
        alias_margin: diagnostics.alias_margin,
        candidates: diagnostics
            .candidates
            .iter()
            .map(|candidate| NeuralCandidateDiagnostics {
                bpm: candidate.bpm,
                relation: neural_relation_name(candidate.relation),
                period_frames: candidate.period_frames,
                available: candidate.available,
                normalized_weight: candidate.normalized_weight,
                best_phase_frames: candidate.best_phase_frames,
                activation_evidence: candidate.activation_evidence,
                coverage: candidate.coverage,
                off_grid_leakage: candidate.off_grid_leakage,
                periodic_consistency: candidate.periodic_consistency,
                candidate_score: candidate.candidate_score,
            })
            .collect(),
        decoder_accepted: diagnostics.decoder_accepted,
        rejection_reason: diagnostics.rejection_reason.clone(),
    }
}

fn neural_relation_name(relation: wotoha_core::beat_analysis::TempoRelation) -> String {
    match relation {
        wotoha_core::beat_analysis::TempoRelation::Primary => "primary",
        wotoha_core::beat_analysis::TempoRelation::HalfTime => "half_time",
        wotoha_core::beat_analysis::TempoRelation::DoubleTime => "double_time",
        wotoha_core::beat_analysis::TempoRelation::Alternative => "alternative",
    }
    .into()
}

type BackendAnalysisResult = Result<
    (
        NormalizedAnalysis,
        &'static str,
        Option<NeuralDiagnostics>,
        Option<wotoha_core::beat_analysis::NeuralBeatObservations>,
    ),
    LabError,
>;

fn analyze_fixture_with_backend(
    fixture: &SyntheticFixture,
    mode: AnalyzerMode,
) -> BackendAnalysisResult {
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
    let (v2, backend, neural_diagnostics, activation_observations) = if mode == AnalyzerMode::Hybrid
    {
        let low_band = low_band_1khz(&analysis_audio);
        let diagnostic_result = wotoha_runtime::analyze_neural_rhythm_with_diagnostics(
            &analysis_audio,
            DEFAULT_SAMPLE_RATE,
            &low_band,
        );
        let diagnostics = normalize_neural_diagnostics(&diagnostic_result.diagnostics);
        let observations = diagnostic_result.observations;
        if let Some(rhythm) = diagnostic_result.rhythm {
            (
                wotoha_runtime::track_analysis_v2_from_legacy_rhythm(&legacy, rhythm, true),
                "native_neural",
                Some(diagnostics),
                observations,
            )
        } else {
            (
                wotoha_runtime::track_analysis_v2_from_legacy(&legacy),
                "classical_fallback",
                Some(diagnostics),
                observations,
            )
        }
    } else {
        (
            wotoha_runtime::track_analysis_v2_from_legacy(&legacy),
            "classical",
            None,
            None,
        )
    };
    let v2 = v2.ok_or_else(|| {
        LabError::InvalidInput(format!("Wotoha V2 adaptation rejected {}", fixture.spec.id))
    })?;
    Ok((
        normalize_v2(&v2),
        backend,
        neural_diagnostics,
        activation_observations,
    ))
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
    #[serde(default)]
    pub source_commit: Option<String>,
    #[serde(default)]
    pub include_backend_comparison: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EvaluationReport {
    pub schema_version: u32,
    pub evaluator: String,
    pub split: String,
    pub analyzer_mode: AnalyzerMode,
    pub source_commit: Option<String>,
    pub analysis_backends: BTreeMap<String, usize>,
    pub overall: GroupMetrics,
    pub by_fixture_family: BTreeMap<String, GroupMetrics>,
    pub by_tempo_range: BTreeMap<String, GroupMetrics>,
    pub by_meter: BTreeMap<String, GroupMetrics>,
    pub by_transform: BTreeMap<String, TransformMetrics>,
    pub transform_invariance: BTreeMap<String, TransformMetrics>,
    pub evidence_ablation: BTreeMap<String, TransformMetrics>,
    pub backend_comparison: BackendComparisonReport,
    pub tempo_experiment: ExperimentalTempoReport,
    pub half_double_errors: BTreeMap<String, usize>,
    pub downbeat_errors: BTreeMap<String, usize>,
    pub variable_tempo: VariableTempoMetrics,
    pub confidence_calibration: Vec<CalibrationBin>,
    pub failure_clusters: BTreeMap<String, usize>,
    pub external: ExternalReport,
    pub per_track: Vec<TrackEvaluation>,
    #[serde(skip)]
    pub research_raw_observations:
        BTreeMap<String, wotoha_core::beat_analysis::NeuralBeatObservations>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BackendComparisonReport {
    pub outcome_counts: BTreeMap<String, usize>,
    pub by_fixture_family: BTreeMap<String, BTreeMap<String, usize>>,
    pub per_fixture: Vec<BackendFixtureComparison>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BackendFixtureComparison {
    pub sample_id: String,
    pub family: String,
    pub hybrid_backend: String,
    pub hybrid: BackendMetricSnapshot,
    pub classical: BackendMetricSnapshot,
    pub deltas: BackendMetricDelta,
    pub outcome: String,
    #[serde(skip)]
    pub hybrid_metrics: GroupMetrics,
    #[serde(skip)]
    pub classical_metrics: GroupMetrics,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BackendMetricSnapshot {
    pub beat_mae_ms: Option<f64>,
    pub beat_p50_ms: Option<f64>,
    pub beat_p95_ms: Option<f64>,
    pub precision_at_40ms: Option<f64>,
    pub recall_at_40ms: Option<f64>,
    pub tempo_absolute_error_bpm: Option<f64>,
    pub primary_tempo_correct: Option<bool>,
    pub grid_phase_error_ms: Option<f64>,
    pub grid_phase_correct: Option<bool>,
    pub downbeat_status: String,
    pub meter_status: String,
    pub core_valid: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BackendMetricDelta {
    pub beat_mae_ms: Option<f64>,
    pub beat_p95_ms: Option<f64>,
    pub precision_at_40ms: Option<f64>,
    pub recall_at_40ms: Option<f64>,
}

struct BackendRunSet {
    hybrid: NormalizedAnalysis,
    hybrid_backend: String,
    classical: NormalizedAnalysis,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ExperimentalTempoReport {
    pub applicable_tracks: usize,
    pub current_primary_correct_count: usize,
    pub experimental_primary_correct_count: usize,
    pub activation_primary_correct_count: usize,
    pub current_half_time_count: usize,
    pub experimental_half_time_count: usize,
    pub activation_half_time_count: usize,
    pub current_double_time_count: usize,
    pub experimental_double_time_count: usize,
    pub activation_double_time_count: usize,
    pub experimental_ambiguous_count: usize,
    pub activation_ambiguous_count: usize,
    pub experimental_wrong_primary_count: usize,
    pub activation_wrong_primary_count: usize,
    pub pcm_regressions_vs_production: Vec<String>,
    pub activation_regressions_vs_production: Vec<String>,
    pub by_fixture_family: BTreeMap<String, ExperimentalTempoFamilySummary>,
    pub per_fixture: Vec<ExperimentalTempoFixture>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TempoExperimentReportDocument {
    pub schema_version: u32,
    pub evaluator: String,
    pub split: String,
    pub analyzer_mode: AnalyzerMode,
    pub source_commit: Option<String>,
    pub experiment: ExperimentalTempoReport,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TempoRefinementCase {
    pub sample_id: String,
    pub family: String,
    pub current_backend: String,
    pub primary_cohort: bool,
    pub exclusion_reason: Option<String>,
    pub truth_bpm: f32,
    pub current_selected_bpm: Option<f32>,
    pub refined_bpm: Option<f32>,
    pub current_absolute_error_bpm: Option<f32>,
    pub refined_absolute_error_bpm: Option<f32>,
    pub selection_relation_before: String,
    pub selection_relation_after: String,
    pub truth_relation_before: String,
    pub truth_relation_after: String,
    pub refinement: Option<FractionalTempoRefinement>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TempoRefinementGroupSummary {
    pub tracks: usize,
    pub excluded_tracks: usize,
    pub current_mean_absolute_error_bpm: Option<f64>,
    pub refined_mean_absolute_error_bpm: Option<f64>,
    pub current_correct: usize,
    pub refined_correct: usize,
    pub half_time_count: usize,
    pub double_time_count: usize,
    pub other_wrong_count: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TempoRefinementReport {
    pub schema_version: u32,
    pub evaluator: String,
    pub split: String,
    pub source_commit: Option<String>,
    pub algorithm: String,
    pub fractional_resolution_frames: f32,
    pub primary_cohort_size: usize,
    pub fallback_excluded_count: usize,
    pub current_neural_mean_absolute_error_bpm: Option<f64>,
    pub refined_mean_absolute_error_bpm: Option<f64>,
    pub current_median_absolute_error_bpm: Option<f64>,
    pub current_p95_absolute_error_bpm: Option<f64>,
    pub refined_median_absolute_error_bpm: Option<f64>,
    pub refined_p95_absolute_error_bpm: Option<f64>,
    pub current_canonical_correctness: usize,
    pub refined_canonical_correctness: usize,
    pub current_half_time_count: usize,
    pub refined_half_time_count: usize,
    pub current_double_time_count: usize,
    pub refined_double_time_count: usize,
    pub current_other_wrong_count: usize,
    pub refined_other_wrong_count: usize,
    pub by_fixture_family: BTreeMap<String, TempoRefinementGroupSummary>,
    pub required_cases: BTreeMap<String, TempoRefinementCase>,
    pub per_fixture: Vec<TempoRefinementCase>,
    pub variable_tempo_excluded_from_global_bpm: bool,
    pub beat_events_changed: bool,
    pub production_recommendation: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ExperimentalTempoFamilySummary {
    pub tracks: usize,
    pub current_primary_correct_count: usize,
    pub experimental_primary_correct_count: usize,
    pub activation_primary_correct_count: usize,
    pub experimental_ambiguous_count: usize,
    pub activation_ambiguous_count: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExperimentalTempoFixture {
    pub sample_id: String,
    pub family: String,
    pub truth_bpm: Option<f32>,
    pub current_primary_bpm: Option<f32>,
    /// Relation of the production result to known truth. This is never an
    /// internal candidate label.
    pub current_relation_to_truth: String,
    /// Relation of the PCM-envelope experiment to known truth.
    pub pcm_relation_to_truth: String,
    /// Relation of the activation experiment to known truth, when raw
    /// activations were available.
    pub activation_relation_to_truth: Option<String>,
    pub pcm: ExperimentalTempoResolution,
    pub activation: Option<ActivationTempoResolution>,
    pub fractional_refinement: Option<FractionalTempoRefinement>,
    #[serde(skip)]
    raw_observations: Option<wotoha_core::beat_analysis::NeuralBeatObservations>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ExperimentalTempoResolution {
    pub selected_bpm: Option<f32>,
    pub selected_relation: Option<String>,
    pub ambiguous: bool,
    pub candidates: Vec<ExperimentalTempoCandidate>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExperimentalTempoCandidate {
    pub bpm: f32,
    pub relation: String,
    pub score: f32,
    pub phase_micros: u64,
    pub activation_support: f32,
    pub coverage: f32,
    pub periodic_consistency: f32,
    pub available: bool,
}

/// Research-only tempo resolution over the raw Beat This activation domain.
/// Candidate evidence is computed independently for each period and is never
/// fed back into production decoding or tempo selection.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ActivationTempoResolution {
    pub selected_bpm: Option<f32>,
    pub selected_relation: Option<String>,
    pub ambiguous: bool,
    pub candidates: Vec<ActivationTempoCandidate>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActivationTempoCandidate {
    pub bpm: f32,
    pub relation: String,
    pub period_frames: usize,
    pub available: bool,
    pub best_phase_frames: Option<usize>,
    pub activation_evidence: f32,
    pub coverage: f32,
    pub off_grid_leakage: f32,
    pub periodic_consistency: f32,
    pub candidate_score: f32,
}

/// Research-only sub-frame tempo refinement. It changes a tempo label only;
/// it never supplies replacement BeatEvent timestamps to production analysis.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FractionalTempoRefinement {
    pub selected_bpm: Option<f32>,
    pub selected_period_frames: Option<f32>,
    pub selected_phase_frames: Option<f32>,
    pub relation: String,
    pub score: f32,
    pub activation_support: f32,
    pub coverage: f32,
    pub off_grid_leakage: f32,
    pub periodic_consistency: f32,
    pub phase_stability: f32,
    pub resolution_frames: f32,
}

/// Research-only tempo estimate derived from the decoded neural event clock.
/// The estimate is a label; it never replaces the Classical or Neural beat
/// timeline used by an analyzer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BeatEventIntervalRefinement {
    pub selected_bpm: Option<f32>,
    pub selected_period_micros: Option<f64>,
    pub relation: String,
    pub usable_events: usize,
    pub usable_intervals: usize,
    pub interval_median_micros: Option<f64>,
    pub interval_mad_micros: Option<f64>,
    pub relative_dispersion: Option<f64>,
    pub fit_residual_micros: Option<f64>,
    pub middle_period_micros: Option<f64>,
    pub early_late_drift: Option<f64>,
    pub available: bool,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TempoAdvisorMetricSummary {
    pub scored: usize,
    pub canonical_correct: usize,
    pub canonical_accuracy: Option<f64>,
    pub half_time: usize,
    pub double_time: usize,
    pub other_wrong: usize,
    pub absent: usize,
    pub absolute_bpm_mae: Option<f64>,
    pub absolute_bpm_median: Option<f64>,
    pub absolute_bpm_p95: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TempoAdvisorOracleComparison {
    pub classical: TempoAdvisorMetricSummary,
    pub candidate: TempoAdvisorMetricSummary,
    pub oracle: TempoAdvisorMetricSummary,
    pub oracle_uplift_over_classical: i64,
    pub classical_rescues_candidate_cannot_provide: Vec<String>,
    pub candidate_rescues_classical: Vec<String>,
    pub both_correct: Vec<String>,
    pub both_wrong: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TempoAdvisorFeatureDescription {
    pub name: String,
    pub definition: String,
    pub source: String,
    pub production_time_available: bool,
    pub missing_value_behavior: String,
    pub observed_min: Option<f64>,
    pub observed_median: Option<f64>,
    pub observed_max: Option<f64>,
    pub exposure: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TempoAdvisorFoldAudit {
    pub fold: usize,
    pub validation_sample_ids: Vec<String>,
    pub training_sample_ids: Vec<String>,
    pub validation_groups: Vec<String>,
    pub training_groups: Vec<String>,
    pub selected_threshold: f64,
    pub selected_candidate: String,
    pub exact_pcm_overlap: bool,
    pub lineage_overlap: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TempoAdvisorFixture {
    pub sample_id: String,
    pub family: String,
    pub truth_bpm: f32,
    pub classical_bpm: Option<f32>,
    pub classical_relation: String,
    pub current_neural_bpm: Option<f32>,
    pub current_neural_relation: String,
    pub activation_refined_bpm: Option<f32>,
    pub activation_refined_relation: String,
    pub event_refined_bpm: Option<f32>,
    pub event_refined_relation: String,
    pub consensus_bpm: Option<f32>,
    pub consensus_relation: String,
    pub oracle_choice: String,
    pub oof_advisor_choice: String,
    pub oof_advisor_selected_bpm: Option<f32>,
    pub oof_advisor_correct: Option<bool>,
    pub advisor_features: BTreeMap<String, f64>,
    pub outer_fold: Option<usize>,
    pub event_refinement: Option<BeatEventIntervalRefinement>,
    pub neural_diagnostics: Option<NeuralDiagnostics>,
    pub classical_tempo_hypotheses: Vec<NormalizedTempoHypothesis>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TempoAdvisorFamilyStress {
    pub requested_family: String,
    pub expanded_validation_samples: Vec<String>,
    pub other_families_pulled_into_validation: Vec<String>,
    pub train_size: usize,
    pub validation_size: usize,
    pub exact_pcm_overlap: bool,
    pub lineage_overlap: bool,
    pub advisor_metrics: TempoAdvisorMetricSummary,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TempoAdvisorReport {
    pub schema_version: u32,
    pub evaluator: String,
    pub source_commit: String,
    pub starting_commit: Option<String>,
    pub corpus_seed: u64,
    pub fixture_count: usize,
    pub scalar_tempo_fixture_count: usize,
    pub native_neural_scalar_cohort: usize,
    pub architecture: String,
    pub baseline_classical: TempoAdvisorMetricSummary,
    pub baseline_current_neural: TempoAdvisorMetricSummary,
    pub activation_refined: TempoAdvisorMetricSummary,
    pub event_refined: TempoAdvisorMetricSummary,
    pub consensus_refined: TempoAdvisorMetricSummary,
    pub refined_oracle: TempoAdvisorMetricSummary,
    pub activation_oracle: TempoAdvisorOracleComparison,
    pub event_oracle: TempoAdvisorOracleComparison,
    pub best_refined_oracle: TempoAdvisorOracleComparison,
    pub classical_failure_budget: Vec<TempoAdvisorFixture>,
    pub feature_inventory: Vec<TempoAdvisorFeatureDescription>,
    pub nested_oof: TempoAdvisorNestedOof,
    pub component_expanded_family_stress: Vec<TempoAdvisorFamilyStress>,
    pub per_fixture: Vec<TempoAdvisorFixture>,
    pub variable_tempo_safety: Vec<TempoAdvisorVariableTempoSafety>,
    pub transform_robustness: BTreeMap<String, TempoAdvisorTransformPair>,
    pub recommendation: String,
    pub production_changes: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TempoAdvisorNestedOof {
    pub grouping_rule: String,
    pub feature_set_frozen_before_oof: bool,
    pub outer_fold_count: usize,
    pub outer_folds: Vec<TempoAdvisorFoldAudit>,
    pub advisor_metrics: TempoAdvisorMetricSummary,
    pub false_accept_sample_ids: Vec<String>,
    pub canonical_rescue_sample_ids: Vec<String>,
    pub neural_coverage: usize,
    pub abstention_count: usize,
    pub full_data_refit_threshold: f64,
    pub full_data_refit_is_not_held_out: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TempoAdvisorVariableTempoSafety {
    pub sample_id: String,
    pub family: String,
    pub fit_residual_micros: Option<f64>,
    pub interval_dispersion: Option<f64>,
    pub early_period_micros: Option<f64>,
    pub middle_period_micros: Option<f64>,
    pub late_period_micros: Option<f64>,
    pub refinement_available: bool,
    pub abstention_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TempoAdvisorTransformPair {
    pub base_sample_id: String,
    pub transformed_sample_id: String,
    pub activation_bpm_delta: Option<f32>,
    pub event_bpm_delta: Option<f32>,
    pub advisor_decision_changed: bool,
}

#[derive(Clone, Debug, Default)]
struct TempoResolverResults {
    pcm: ExperimentalTempoResolution,
    activation: Option<ActivationTempoResolution>,
    raw_observations: Option<wotoha_core::beat_analysis::NeuralBeatObservations>,
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
    pub scored_tracks: usize,
    pub unobserved_tracks: usize,
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
    pub expectation: TransformExpectation,
    pub tracks: usize,
    pub base_beat_count: usize,
    pub transformed_beat_count: usize,
    pub matched_beats: usize,
    pub missing_beats: usize,
    pub extra_beats: usize,
    pub mean_beat_displacement_ms: Option<f64>,
    pub p95_beat_displacement_ms: Option<f64>,
    pub tempo_before_bpm: Option<f32>,
    pub tempo_after_bpm: Option<f32>,
    pub backend_before: Option<String>,
    pub backend_after: Option<String>,
    pub low_frequency_support_change: Option<f64>,
    pub confidence_before: Option<f64>,
    pub confidence_after: Option<f64>,
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
    pub platform: String,
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
    #[serde(rename = "input_artifact_sha256")]
    pub audio_sha256: String,
    pub analysis_backend: String,
    pub neural_diagnostics: Option<NeuralDiagnostics>,
    pub meter_evidence: MeterEvidenceReport,
    pub truth: AnalysisGroundTruth,
    pub wotoha: NormalizedAnalysis,
    pub metrics: GroupMetrics,
    pub failure_clusters: Vec<String>,
    pub human_review: Option<HumanReviewLabel>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MeterEvidenceReport {
    pub candidates: BTreeMap<String, MeterCandidateEvidence>,
    pub resolved_meter: Option<u8>,
    pub runner_up_margin: Option<f32>,
    pub downbeat_confidence: Option<f32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MeterCandidateEvidence {
    pub phase: Option<u8>,
    pub score: Option<f32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NeuralDiagnostics {
    pub selected_period_frames: Option<usize>,
    pub selected_bpm: Option<f32>,
    pub path_marker_count: usize,
    pub path_coverage: Option<f32>,
    pub activation_mean: Option<f32>,
    pub support: Option<f32>,
    pub interval_residual: Option<f32>,
    pub alias_margin: Option<f32>,
    pub candidates: Vec<NeuralCandidateDiagnostics>,
    pub decoder_accepted: bool,
    pub rejection_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NeuralCandidateDiagnostics {
    pub bpm: f32,
    pub relation: String,
    pub period_frames: usize,
    pub available: bool,
    pub normalized_weight: Option<f32>,
    pub best_phase_frames: Option<usize>,
    pub activation_evidence: f32,
    pub coverage: f32,
    pub off_grid_leakage: f32,
    pub periodic_consistency: f32,
    pub candidate_score: f32,
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
    evaluate_manifest_with_audio(manifest, external, options, &AtomicBool::new(false), None)
}

pub fn evaluate_manifest_with_cancel(
    manifest: &SyntheticCorpusManifest,
    external: Option<&ExternalObservationDocument>,
    options: EvaluationOptions,
    cancelled: &AtomicBool,
) -> Result<EvaluationReport, LabError> {
    evaluate_manifest_with_audio(manifest, external, options, cancelled, None)
}

pub fn evaluate_manifest_with_audio(
    manifest: &SyntheticCorpusManifest,
    external: Option<&ExternalObservationDocument>,
    options: EvaluationOptions,
    cancelled: &AtomicBool,
    audio_overrides: Option<&BTreeMap<String, Vec<f32>>>,
) -> Result<EvaluationReport, LabError> {
    evaluate_manifest_with_context(
        manifest,
        external,
        options,
        cancelled,
        audio_overrides,
        None,
    )
}

#[derive(Clone, Debug)]
struct ExternalIdentity {
    wav_file_sha256: String,
    pcm_sha256: String,
    ground_truth_sha256: String,
}

fn evaluate_manifest_with_context(
    manifest: &SyntheticCorpusManifest,
    external: Option<&ExternalObservationDocument>,
    options: EvaluationOptions,
    cancelled: &AtomicBool,
    audio_overrides: Option<&BTreeMap<String, Vec<f32>>>,
    external_identities: Option<&BTreeMap<String, ExternalIdentity>>,
) -> Result<EvaluationReport, LabError> {
    if audio_overrides.is_some() {
        manifest.validate_structure()?;
    } else {
        manifest.validate()?;
    }
    if let Some(external) = external {
        if external_identities.is_none() {
            return Err(LabError::InvalidInput(
                "external observations are exported-WAV-only; use evaluate-exported with a BlackboxManifest".into(),
            ));
        }
        external.validate()?;
    }
    let mut tracks = Vec::with_capacity(manifest.fixtures.len());
    let mut normalized_by_id = BTreeMap::new();
    let mut backends_by_id = BTreeMap::new();
    let mut backend_runs = BTreeMap::new();
    let mut experimental_results = BTreeMap::new();
    for record in &manifest.fixtures {
        if cancelled.load(Ordering::Relaxed) {
            return Err(LabError::Cancelled);
        }
        let fixture =
            if let Some(audio) = audio_overrides.and_then(|items| items.get(&record.spec.id)) {
                if audio.len() > MAX_AUDIO_SAMPLES {
                    return Err(LabError::InvalidInput(format!(
                        "exported fixture {} exceeds bounded audio memory",
                        record.spec.id
                    )));
                }
                SyntheticFixture {
                    spec: record.spec.clone(),
                    audio: audio.clone(),
                    truth: record.truth.clone(),
                    audio_sha256: hash_pcm(audio),
                }
            } else {
                generate_fixture(&record.spec)?
            };
        if audio_overrides.is_none() && fixture.audio_sha256 != record.audio_sha256 {
            return Err(LabError::HashMismatch {
                sample_id: record.spec.id.clone(),
                expected: record.audio_sha256.clone(),
                actual: fixture.audio_sha256,
            });
        }
        let (wotoha, analysis_backend, neural_diagnostics, mut activation_observations) =
            analyze_fixture_with_backend(&fixture, options.mode)?;
        if options.include_backend_comparison {
            let (hybrid, hybrid_backend, classical) = match options.mode {
                AnalyzerMode::Hybrid => {
                    let (classical, _, _, _) =
                        analyze_fixture_with_backend(&fixture, AnalyzerMode::Classical)?;
                    (wotoha.clone(), analysis_backend.to_string(), classical)
                }
                AnalyzerMode::Classical => {
                    let (hybrid, hybrid_backend, _, observations) =
                        analyze_fixture_with_backend(&fixture, AnalyzerMode::Hybrid)?;
                    activation_observations = observations;
                    (hybrid, hybrid_backend.to_string(), wotoha.clone())
                }
            };
            backend_runs.insert(
                record.spec.id.clone(),
                BackendRunSet {
                    hybrid,
                    hybrid_backend,
                    classical,
                },
            );
        }
        normalized_by_id.insert(record.spec.id.clone(), wotoha.clone());
        backends_by_id.insert(record.spec.id.clone(), analysis_backend.to_owned());
        experimental_results.insert(record.spec.id.clone(), {
            let activation = activation_observations.as_ref().and_then(|observations| {
                activation_tempo_resolution(observations, primary_tempo_bpm(&wotoha))
            });
            TempoResolverResults {
                pcm: experimental_tempo_resolution(&fixture, &wotoha),
                activation,
                raw_observations: activation_observations.clone(),
            }
        });
        let metrics = metrics_for(&record.truth, &wotoha);
        let failures = failure_clusters(&record.truth, &wotoha, &metrics);
        tracks.push(TrackEvaluation {
            sample_id: record.spec.id.clone(),
            family: record.spec.family.as_str().into(),
            audio_sha256: record.audio_sha256.clone(),
            analysis_backend: analysis_backend.into(),
            neural_diagnostics,
            meter_evidence: meter_evidence(&wotoha),
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
    let by_transform = transform_metrics(&manifest.fixtures, &normalized_by_id, &backends_by_id);
    let transform_invariance = by_transform
        .iter()
        .filter(|(_, metrics)| metrics.expectation == TransformExpectation::Invariant)
        .map(|(key, metrics)| (key.clone(), metrics.clone()))
        .collect();
    let evidence_ablation = by_transform
        .iter()
        .filter(|(_, metrics)| metrics.expectation == TransformExpectation::EvidenceAblation)
        .map(|(key, metrics)| (key.clone(), metrics.clone()))
        .collect();
    let backend_comparison = backend_comparison(manifest, &backend_runs);
    let tempo_experiment =
        experimental_tempo_report(manifest, &experimental_results, &normalized_by_id);
    let (half_double_errors, downbeat_errors, failure_clusters) = summarize_failures(&tracks);
    let confidence_calibration = calibration(&tracks);
    let variable_tempo = variable_tempo_metrics(&tracks);
    let mut analysis_backends = BTreeMap::new();
    for track in &tracks {
        *analysis_backends
            .entry(track.analysis_backend.clone())
            .or_insert(0) += 1;
    }
    let analyzer_mode = options.mode;
    let source_commit = options.source_commit;
    let research_raw_observations = experimental_results
        .iter()
        .filter_map(|(sample_id, result)| {
            result
                .raw_observations
                .clone()
                .map(|observations| (sample_id.clone(), observations))
        })
        .collect();
    let external_report = external
        .map(|document| compare_external(document, manifest, &tracks, external_identities))
        .transpose()?
        .unwrap_or_default();
    Ok(EvaluationReport {
        schema_version: REPORT_SCHEMA_VERSION,
        evaluator: format!("wotoha-analysis-lab/{}", env!("CARGO_PKG_VERSION")),
        split: options.split,
        analyzer_mode,
        source_commit,
        analysis_backends,
        overall,
        by_fixture_family,
        by_tempo_range,
        by_meter,
        by_transform,
        transform_invariance,
        evidence_ablation,
        backend_comparison,
        tempo_experiment,
        half_double_errors,
        downbeat_errors,
        variable_tempo,
        confidence_calibration,
        failure_clusters,
        external: external_report,
        per_track: tracks,
        research_raw_observations,
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

fn meter_evidence(prediction: &NormalizedAnalysis) -> MeterEvidenceReport {
    let mut candidates = BTreeMap::new();
    let mut scores = Vec::new();
    for meter in [2_u8, 3, 4, 6] {
        let evidence = prediction
            .meter_hypotheses
            .iter()
            .find(|hypothesis| hypothesis.beats_per_bar == meter)
            .map(|hypothesis| {
                scores.push(hypothesis.score);
                MeterCandidateEvidence {
                    phase: Some(hypothesis.downbeat_phase),
                    score: Some(hypothesis.score),
                }
            })
            .unwrap_or(MeterCandidateEvidence {
                phase: None,
                score: None,
            });
        candidates.insert(meter.to_string(), evidence);
    }
    scores.sort_by(f32::total_cmp);
    let runner_up_margin = scores
        .last()
        .zip(scores.iter().rev().nth(1))
        .map(|(best, runner_up)| best - runner_up);
    MeterEvidenceReport {
        candidates,
        resolved_meter: prediction.resolved_meter,
        runner_up_margin,
        downbeat_confidence: prediction
            .confidence
            .downbeat
            .iter()
            .copied()
            .reduce(f32::max),
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MeterPhaseCandidateResearch {
    pub meter: u8,
    pub phase: u8,
    pub target_phase_downbeat_evidence: f32,
    pub off_phase_downbeat_leakage: f32,
    pub accent_periodic_consistency: f32,
    pub bar_cycle_consistency: f32,
    pub beat_event_confidence: f32,
    pub score: f32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MeterResearchFixture {
    pub sample_id: String,
    pub truth: Option<u8>,
    pub current_resolved_meter: Option<u8>,
    pub experimental_resolved_meter: Option<u8>,
    pub current_hypotheses: BTreeMap<String, MeterCandidateEvidence>,
    pub experimental_candidates: Vec<MeterPhaseCandidateResearch>,
    pub best_score: Option<f32>,
    pub runner_up_score: Option<f32>,
    pub margin: Option<f32>,
    pub unknown_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MeterResearchReport {
    pub schema_version: u32,
    pub evaluator: String,
    pub split: String,
    pub source_commit: Option<String>,
    pub candidate_meters: Vec<u8>,
    pub candidate_count: usize,
    pub clear_fixture_count: usize,
    pub current_clear_correct: usize,
    pub current_clear_unknown: usize,
    pub current_clear_wrong: usize,
    pub experimental_clear_correct: usize,
    pub experimental_clear_unknown: usize,
    pub experimental_clear_wrong: usize,
    pub ambiguous_fixture_count: usize,
    pub ambiguous_resolved_count: usize,
    pub ambiguous_unknown_count: usize,
    pub ambiguous_fixtures: Vec<MeterResearchFixture>,
    pub experimental_recovers_non4_meter_count: usize,
    pub experimental_recovers_non4_meter_fixtures: Vec<String>,
    pub current_non4_meter_failures: Vec<String>,
    pub four_phase_prior_interpretation: String,
    pub per_fixture: Vec<MeterResearchFixture>,
    pub production_recommendation: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FailureLineageRecord {
    pub sample_id: String,
    pub production_backend: String,
    pub classical_core_valid: bool,
    pub neural_core_valid: bool,
    pub backend_outcome: String,
    pub tempo_issue: String,
    pub beat_issue: String,
    pub meter_issue: String,
    pub research_gate_decision: String,
    pub oracle_decision: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResearchSummary {
    pub schema_version: u32,
    pub evaluator: String,
    pub split: String,
    pub source_commit: Option<String>,
    pub starting_commit: Option<String>,
    pub corpus_seed: u64,
    pub fixture_count: usize,
    pub current_classical: ResearchBackendSummary,
    pub current_hybrid: ResearchBackendSummary,
    pub backend_outcome_counts: BTreeMap<String, usize>,
    pub oracle_joint_outcome_counts: BTreeMap<String, usize>,
    pub gate_production_recommendation: String,
    pub tempo_refinement_production_recommendation: String,
    pub meter_production_recommendation: String,
    pub old_pcm_experiment_primary_correct: usize,
    pub old_activation_experiment_primary_correct: usize,
    pub failure_lineage: Vec<FailureLineageRecord>,
    pub scope_guarantees: Vec<String>,
}

fn meter_phase_candidate(
    prediction: &NormalizedAnalysis,
    meter: u8,
    phase: u8,
) -> MeterPhaseCandidateResearch {
    let mut target = Vec::new();
    let mut off_phase = Vec::new();
    let mut target_confidence = Vec::new();
    for (index, beat) in prediction.beats.iter().enumerate() {
        if index % usize::from(meter) == usize::from(phase) {
            target.push(beat.downbeat_evidence.unwrap_or_default());
            target_confidence.push(beat.timing_confidence);
        } else {
            off_phase.push(beat.downbeat_evidence.unwrap_or_default());
        }
    }
    let target_evidence = mean_f32(&target);
    let leakage = mean_f32(&off_phase).unwrap_or_default();
    let target_mean = target_evidence.unwrap_or_default();
    let periodic = if target.len() > 1 {
        let mean = target_mean;
        (1.0 - target.iter().map(|value| (value - mean).abs()).sum::<f32>() / target.len() as f32)
            .clamp(0.0, 1.0)
    } else {
        0.0
    };
    let bar_cycle = if target.len() >= 2 && prediction.beats.len() >= usize::from(meter) * 2 {
        let first = target.iter().step_by(2).copied().collect::<Vec<_>>();
        let second = target
            .iter()
            .skip(1)
            .step_by(2)
            .copied()
            .collect::<Vec<_>>();
        (1.0 - (mean_f32(&first).unwrap_or_default() - mean_f32(&second).unwrap_or_default()).abs())
            .clamp(0.0, 1.0)
    } else {
        0.0
    };
    let confidence = mean_f32(&target_confidence).unwrap_or_default();
    let score =
        (target_mean - 1.80 * leakage + 0.20 * periodic + 0.15 * bar_cycle + 0.10 * confidence)
            .max(-1.0);
    MeterPhaseCandidateResearch {
        meter,
        phase,
        target_phase_downbeat_evidence: target_mean,
        off_phase_downbeat_leakage: leakage,
        accent_periodic_consistency: periodic,
        bar_cycle_consistency: bar_cycle,
        beat_event_confidence: confidence,
        score,
    }
}

fn mean_f32(values: &[f32]) -> Option<f32> {
    (!values.is_empty()).then(|| values.iter().sum::<f32>() / values.len() as f32)
}

fn experimental_meter_resolution(
    prediction: &NormalizedAnalysis,
) -> (
    Option<u8>,
    Vec<MeterPhaseCandidateResearch>,
    Option<f32>,
    Option<String>,
) {
    let mut candidates = Vec::new();
    for meter in [2_u8, 3, 4, 6] {
        for phase in 0..meter {
            candidates.push(meter_phase_candidate(prediction, meter, phase));
        }
    }
    candidates.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.meter.cmp(&right.meter))
            .then_with(|| left.phase.cmp(&right.phase))
    });
    let best = candidates.first();
    let runner_up = candidates.get(1);
    let margin = best
        .zip(runner_up)
        .map(|(best, runner)| best.score - runner.score);
    let unknown_reason = if best.is_none() {
        Some("no beat events".into())
    } else if best.is_some_and(|candidate| candidate.score < 0.18) {
        Some("best meter×phase score below minimum".into())
    } else if margin.is_some_and(|value| value < 0.04) {
        Some("meter×phase candidates are too close".into())
    } else {
        None
    };
    let resolved = unknown_reason.is_none().then(|| best.unwrap().meter);
    (resolved, candidates, margin, unknown_reason)
}

fn build_meter_research(
    report: &EvaluationReport,
    source_commit: Option<String>,
) -> MeterResearchReport {
    let mut per_fixture = Vec::new();
    let mut clear_fixture_count = 0;
    let mut current_clear_correct = 0;
    let mut current_clear_unknown = 0;
    let mut current_clear_wrong = 0;
    let mut experimental_clear_correct = 0;
    let mut experimental_clear_unknown = 0;
    let mut experimental_clear_wrong = 0;
    let mut ambiguous_fixtures = Vec::new();
    for track in &report.per_track {
        if !is_meter_research_family(&track.family) {
            continue;
        }
        let (experimental, candidates, margin, unknown_reason) =
            experimental_meter_resolution(&track.wotoha);
        let best_score = candidates.first().map(|candidate| candidate.score);
        let runner_up_score = candidates.get(1).map(|candidate| candidate.score);
        let fixture = MeterResearchFixture {
            sample_id: track.sample_id.clone(),
            truth: track.truth.meter,
            current_resolved_meter: track.meter_evidence.resolved_meter,
            experimental_resolved_meter: experimental,
            current_hypotheses: track.meter_evidence.candidates.clone(),
            experimental_candidates: candidates,
            best_score,
            runner_up_score,
            margin,
            unknown_reason,
        };
        if !meter_research_truth_is_clear(track.truth.meter) {
            ambiguous_fixtures.push(fixture.clone());
        } else {
            clear_fixture_count += 1;
            match track.meter_evidence.resolved_meter {
                Some(value) if Some(value) == track.truth.meter => current_clear_correct += 1,
                None => current_clear_unknown += 1,
                Some(_) => current_clear_wrong += 1,
            }
            match experimental {
                Some(value) if Some(value) == track.truth.meter => experimental_clear_correct += 1,
                None => experimental_clear_unknown += 1,
                Some(_) => experimental_clear_wrong += 1,
            }
        }
        per_fixture.push(fixture);
    }
    let experimental_recovers_non4_meter_fixtures = per_fixture
        .iter()
        .filter(|fixture| {
            matches!(fixture.truth, Some(3 | 6))
                && fixture.experimental_resolved_meter == fixture.truth
        })
        .map(|fixture| fixture.sample_id.clone())
        .collect::<Vec<_>>();
    let current_non4_meter_failures = per_fixture
        .iter()
        .filter(|fixture| {
            matches!(fixture.truth, Some(3 | 6)) && fixture.current_resolved_meter != fixture.truth
        })
        .map(|fixture| fixture.sample_id.clone())
        .collect::<Vec<_>>();
    let four_phase_prior_interpretation = if experimental_recovers_non4_meter_fixtures.is_empty() {
        "The experimental scorer recovers no non-4 meter fixture; the failure remains upstream-or-downstream ambiguous.".into()
    } else {
        "The downstream scorer recovers non-4 meter fixtures despite the current four-phase prior; this result alone does not identify the prior as causal.".into()
    };
    per_fixture.sort_by(|left, right| left.sample_id.cmp(&right.sample_id));
    let ambiguous_resolved_count = ambiguous_fixtures
        .iter()
        .filter(|fixture| fixture.experimental_resolved_meter.is_some())
        .count();
    let ambiguous_unknown_count = ambiguous_fixtures.len() - ambiguous_resolved_count;
    MeterResearchReport {
        schema_version: RESEARCH_REPORT_SCHEMA_VERSION,
        evaluator: format!("wotoha-analysis-lab/{}", env!("CARGO_PKG_VERSION")),
        split: report.split.clone(),
        source_commit,
        candidate_meters: vec![2, 3, 4, 6],
        candidate_count: 2 + 3 + 4 + 6,
        clear_fixture_count,
        current_clear_correct,
        current_clear_unknown,
        current_clear_wrong,
        experimental_clear_correct,
        experimental_clear_unknown,
        experimental_clear_wrong,
        ambiguous_fixture_count: ambiguous_fixtures.len(),
        ambiguous_resolved_count,
        ambiguous_unknown_count,
        ambiguous_fixtures,
        experimental_recovers_non4_meter_count: experimental_recovers_non4_meter_fixtures.len(),
        experimental_recovers_non4_meter_fixtures,
        current_non4_meter_failures,
        four_phase_prior_interpretation,
        per_fixture,
        production_recommendation:
            "do-not-promote: contrastive meter×phase scorer remains research-only".into(),
    }
}

fn is_meter_research_family(family: &str) -> bool {
    family == FixtureFamily::Meter.as_str()
}

fn meter_research_truth_is_clear(meter: Option<u8>) -> bool {
    meter.is_some()
}

fn lineage_group_map(blackbox: &BlackboxManifest) -> BTreeMap<String, String> {
    let parents = blackbox
        .fixtures
        .iter()
        .map(|fixture| (fixture.sample_id.clone(), fixture.spec.base_id.clone()))
        .collect::<BTreeMap<_, _>>();
    blackbox
        .fixtures
        .iter()
        .map(|fixture| {
            let mut current = fixture.sample_id.clone();
            let mut seen = BTreeSet::new();
            while let Some(Some(parent)) = parents.get(&current) {
                if !seen.insert(current.clone()) {
                    break;
                }
                current = parent.clone();
            }
            (fixture.sample_id.clone(), current)
        })
        .collect()
}

fn build_failure_lineage(
    report: &EvaluationReport,
    oracle: &BackendOracleReport,
    gate: &GateResearchReport,
    tempo: &TempoRefinementReport,
) -> Vec<FailureLineageRecord> {
    let oracle_by_id = oracle
        .per_fixture
        .iter()
        .map(|item| (item.sample_id.as_str(), item))
        .collect::<BTreeMap<_, _>>();
    let gate_by_id = gate
        .oof_decisions
        .iter()
        .map(|item| (item.sample_id.as_str(), item.decision.as_str()))
        .collect::<BTreeMap<_, _>>();
    let tempo_by_id = tempo
        .per_fixture
        .iter()
        .map(|item| (item.sample_id.as_str(), item))
        .collect::<BTreeMap<_, _>>();
    report
        .backend_comparison
        .per_fixture
        .iter()
        .filter_map(|comparison| {
            let track = report
                .per_track
                .iter()
                .find(|track| track.sample_id == comparison.sample_id)?;
            let tempo_issue = tempo_by_id
                .get(comparison.sample_id.as_str())
                .map(|tempo| {
                    match tempo
                        .truth_relation_before
                        .split_whitespace()
                        .next()
                        .unwrap_or("none")
                    {
                        "half_time" => "half_time",
                        "double_time" => "double_time",
                        "primary"
                            if tempo.current_absolute_error_bpm.unwrap_or_default() > 0.25 =>
                        {
                            "frame_quantization"
                        }
                        "primary" | "none" => "none",
                        _ => "other",
                    }
                })
                .unwrap_or("none")
                .into();
            let beat_issue = if track.metrics.beat.predicted == 0 {
                "missing"
            } else if track.metrics.beat.truth == 0 {
                "extra"
            } else if track.metrics.beat.mae_ms.unwrap_or_default() > 10.0 {
                "timing"
            } else {
                "none"
            }
            .into();
            let meter_issue =
                if track.truth.meter.is_some() && track.metrics.meter.status != "correct" {
                    track.metrics.meter.status.clone()
                } else {
                    "none".into()
                };
            let gate_decision = gate_by_id
                .get(comparison.sample_id.as_str())
                .copied()
                .unwrap_or("unknown")
                .into();
            Some(FailureLineageRecord {
                sample_id: comparison.sample_id.clone(),
                production_backend: comparison.hybrid_backend.clone(),
                classical_core_valid: comparison.classical.core_valid,
                neural_core_valid: comparison.hybrid_backend == "native_neural"
                    && comparison.hybrid.core_valid,
                backend_outcome: comparison.outcome.clone(),
                tempo_issue,
                beat_issue,
                meter_issue,
                research_gate_decision: gate_decision,
                oracle_decision: oracle_by_id
                    .get(comparison.sample_id.as_str())
                    .map(|item| item.joint_dominance.clone())
                    .unwrap_or_else(|| "unknown".into()),
            })
        })
        .collect()
}

pub fn run_ground_truth_research(
    manifest_path: &Path,
    audio_root: &Path,
    output_dir: &Path,
    source_commit: String,
    starting_commit: Option<String>,
) -> Result<ResearchSummary, LabError> {
    let blackbox = BlackboxManifest::load(manifest_path)?;
    fs::create_dir_all(output_dir)?;
    let hybrid = evaluate_exported_manifest(
        manifest_path,
        audio_root,
        None,
        EvaluationOptions {
            mode: AnalyzerMode::Hybrid,
            split: blackbox.split.clone(),
            source_commit: Some(source_commit.clone()),
            include_backend_comparison: true,
        },
    )?;
    let classical = evaluate_exported_manifest(
        manifest_path,
        audio_root,
        None,
        EvaluationOptions {
            mode: AnalyzerMode::Classical,
            split: blackbox.split.clone(),
            source_commit: Some(source_commit.clone()),
            include_backend_comparison: false,
        },
    )?;
    write_json(&output_dir.join("current-classical.json"), &classical)?;
    write_json(&output_dir.join("current-hybrid.json"), &hybrid)?;
    let mut oracle = build_backend_oracle(&hybrid, Some(source_commit.clone()));
    oracle.always_classical = summary_from_group_metrics(&classical.overall);
    oracle.current_hybrid = summary_from_group_metrics(&hybrid.overall);
    write_json(&output_dir.join("backend-oracle.json"), &oracle)?;
    let pcm_groups = blackbox
        .fixtures
        .iter()
        .map(|fixture| (fixture.sample_id.clone(), fixture.pcm_sha256.clone()))
        .collect::<BTreeMap<_, _>>();
    let lineage_groups = lineage_group_map(&blackbox);
    let mut gate = build_gate_research(
        &hybrid,
        &pcm_groups,
        &lineage_groups,
        Some(source_commit.clone()),
    );
    gate.always_classical = summary_from_group_metrics(&classical.overall);
    gate.current_hybrid = summary_from_group_metrics(&hybrid.overall);
    write_json(&output_dir.join("gate-research.json"), &gate)?;
    let tempo = build_tempo_refinement_report(&hybrid, Some(source_commit.clone()));
    write_json(&output_dir.join("tempo-refinement.json"), &tempo)?;
    let meter = build_meter_research(&hybrid, Some(source_commit.clone()));
    write_json(&output_dir.join("meter-research.json"), &meter)?;
    let failure_lineage = build_failure_lineage(&hybrid, &oracle, &gate, &tempo);
    let summary = ResearchSummary {
        schema_version: RESEARCH_REPORT_SCHEMA_VERSION,
        evaluator: format!("wotoha-analysis-lab/{}", env!("CARGO_PKG_VERSION")),
        split: hybrid.split.clone(),
        source_commit: Some(source_commit.clone()),
        starting_commit,
        corpus_seed: blackbox.seed,
        fixture_count: blackbox.fixtures.len(),
        current_classical: oracle.always_classical.clone(),
        current_hybrid: oracle.current_hybrid.clone(),
        backend_outcome_counts: hybrid.backend_comparison.outcome_counts.clone(),
        oracle_joint_outcome_counts: oracle.joint_outcome_counts.clone(),
        gate_production_recommendation: gate.production_recommendation.clone(),
        tempo_refinement_production_recommendation: tempo.production_recommendation.clone(),
        meter_production_recommendation: meter.production_recommendation.clone(),
        old_pcm_experiment_primary_correct: hybrid
            .tempo_experiment
            .experimental_primary_correct_count,
        old_activation_experiment_primary_correct: hybrid
            .tempo_experiment
            .activation_primary_correct_count,
        failure_lineage,
        scope_guarantees: vec![
            "production Hybrid gate unchanged".into(),
            "production BeatEvent timing unchanged".into(),
            "production tempo and meter resolvers unchanged".into(),
            "research algorithms are not production-callable".into(),
            "vendor-specific binaries and proprietary resources were not inspected".into(),
            "automatic analysis from external products was not used".into(),
            "reports and WAVs remain outside Git and Docker".into(),
        ],
    };
    write_json(&output_dir.join("research-summary.json"), &summary)?;
    let markdown = research_summary_markdown_corrected(&summary, &oracle, &gate, &tempo, &meter);
    fs::write(output_dir.join("research-summary.md"), markdown)?;
    Ok(summary)
}

/// Run the research-only Classical-rhythm/Neural-tempo-advisor experiment.
///
/// This deliberately evaluates two independent exported-WAV runs and joins
/// only their pre-truth diagnostics. The returned advisor can select a tempo
/// label, but no Neural beat event, phase, meter, or downbeat is ever copied
/// into the hypothetical architecture.
pub fn run_tempo_advisor_research(
    manifest_path: &Path,
    audio_root: &Path,
    output_dir: &Path,
    source_commit: String,
    starting_commit: Option<String>,
) -> Result<TempoAdvisorReport, LabError> {
    let blackbox = BlackboxManifest::load(manifest_path)?;
    fs::create_dir_all(output_dir)?;
    let hybrid = evaluate_exported_manifest(
        manifest_path,
        audio_root,
        None,
        EvaluationOptions {
            mode: AnalyzerMode::Hybrid,
            split: blackbox.split.clone(),
            source_commit: Some(source_commit.clone()),
            include_backend_comparison: true,
        },
    )?;
    let classical = evaluate_exported_manifest(
        manifest_path,
        audio_root,
        None,
        EvaluationOptions {
            mode: AnalyzerMode::Classical,
            split: blackbox.split.clone(),
            source_commit: Some(source_commit.clone()),
            include_backend_comparison: true,
        },
    )?;
    let mut rows = build_tempo_advisor_rows(&blackbox, &hybrid, &classical)?;
    let mut gate_rows = rows
        .iter()
        .map(|row| GateFeatureRow {
            sample_id: row.fixture.sample_id.clone(),
            family: row.fixture.family.clone(),
            pcm_group: row.pcm_group.clone(),
            lineage_group: row.lineage_group.clone(),
            leakage_group: String::new(),
            neural_available: row.candidate_bpm.is_some(),
            features: row.fixture.advisor_features.clone(),
            diagnostic_score: row.diagnostic_score,
            truth_side_label: if canonical_tempo(row.fixture.classical_bpm, row.fixture.truth_bpm)
                && !canonical_tempo(row.candidate_bpm, row.fixture.truth_bpm)
            {
                "classical_dominates"
            } else if !canonical_tempo(row.fixture.classical_bpm, row.fixture.truth_bpm)
                && canonical_tempo(row.candidate_bpm, row.fixture.truth_bpm)
            {
                "neural_dominates"
            } else {
                "equal"
            }
            .into(),
        })
        .collect::<Vec<_>>();
    let leakage_groups = connected_leakage_groups(&gate_rows);
    for row in &mut gate_rows {
        row.leakage_group = leakage_groups
            .get(&row.sample_id)
            .cloned()
            .unwrap_or_else(|| row.sample_id.clone());
    }
    for row in &mut rows {
        row.leakage_group = leakage_groups
            .get(&row.fixture.sample_id)
            .cloned()
            .unwrap_or_else(|| row.fixture.sample_id.clone());
    }
    let nested_oof = run_tempo_advisor_oof(&mut rows);
    let family_stress = build_tempo_advisor_family_stress(&rows, &gate_rows);
    let feature_inventory = build_tempo_advisor_feature_inventory(&rows);
    let activation_values = rows
        .iter()
        .map(|row| {
            (
                row.fixture.sample_id.clone(),
                row.fixture.activation_refined_bpm,
                row.fixture.truth_bpm,
            )
        })
        .collect::<Vec<_>>();
    let event_values = rows
        .iter()
        .map(|row| {
            (
                row.fixture.sample_id.clone(),
                row.fixture.event_refined_bpm,
                row.fixture.truth_bpm,
            )
        })
        .collect::<Vec<_>>();
    let consensus_values = rows
        .iter()
        .map(|row| {
            (
                row.fixture.sample_id.clone(),
                row.fixture.consensus_bpm,
                row.fixture.truth_bpm,
            )
        })
        .collect::<Vec<_>>();
    let current_values = rows
        .iter()
        .map(|row| {
            (
                row.fixture.sample_id.clone(),
                row.fixture.current_neural_bpm,
                row.fixture.truth_bpm,
            )
        })
        .collect::<Vec<_>>();
    let classical_values = rows
        .iter()
        .map(|row| {
            (
                row.fixture.sample_id.clone(),
                row.fixture.classical_bpm,
                row.fixture.truth_bpm,
            )
        })
        .collect::<Vec<_>>();
    let refined_values = rows
        .iter()
        .map(|row| {
            (
                row.fixture.sample_id.clone(),
                best_research_candidate(row).1,
                row.fixture.truth_bpm,
            )
        })
        .collect::<Vec<_>>();
    for row in &mut rows {
        row.fixture.oracle_choice = tempo_oracle_choice(row);
    }
    let activation_oracle = tempo_advisor_oracle(&rows, |row| row.fixture.activation_refined_bpm);
    let event_oracle = tempo_advisor_oracle(&rows, |row| row.fixture.event_refined_bpm);
    let best_refined_oracle = tempo_advisor_oracle(&rows, |row| best_research_candidate(row).1);
    let classical_failures = rows
        .iter()
        .filter(|row| !canonical_tempo(row.fixture.classical_bpm, row.fixture.truth_bpm))
        .map(|row| row.fixture.clone())
        .collect::<Vec<_>>();
    let variable_tempo_safety = build_variable_tempo_safety(&rows, &hybrid);
    let transform_robustness = build_tempo_transform_robustness(&blackbox, &rows);
    let recommendation = if nested_oof.advisor_metrics.canonical_correct
        > tempo_metric_summary(&classical_values).canonical_correct
        && nested_oof.false_accept_sample_ids.is_empty()
    {
        "B — research advisor promising, not ready: the outer grouped result shows a rescue signal, but the synthetic corpus is too small for promotion.".into()
    } else {
        "A — no advisor value: Always Classical remains the rational tempo architecture; retain Neural refinements as research diagnostics only.".into()
    };
    let report = TempoAdvisorReport {
        schema_version: RESEARCH_REPORT_SCHEMA_VERSION + 1,
        evaluator: format!("wotoha-analysis-lab/{}", env!("CARGO_PKG_VERSION")),
        source_commit: source_commit.clone(),
        starting_commit,
        corpus_seed: blackbox.seed,
        fixture_count: blackbox.fixtures.len(),
        scalar_tempo_fixture_count: rows.len(),
        native_neural_scalar_cohort: rows
            .iter()
            .filter(|row| row.fixture.current_neural_bpm.is_some())
            .count(),
        architecture: "Classical beat timeline/grid/meter/downbeat authority plus optional tempo-only Neural advisor".into(),
        baseline_classical: tempo_metric_summary(&classical_values),
        baseline_current_neural: tempo_metric_summary_available(&current_values),
        activation_refined: tempo_metric_summary_available(&activation_values),
        event_refined: tempo_metric_summary_available(&event_values),
        consensus_refined: tempo_metric_summary_available(&consensus_values),
        refined_oracle: tempo_metric_summary_available(&refined_values),
        activation_oracle,
        event_oracle,
        best_refined_oracle,
        classical_failure_budget: classical_failures,
        feature_inventory,
        nested_oof,
        component_expanded_family_stress: family_stress,
        per_fixture: rows.iter().map(|row| row.fixture.clone()).collect(),
        variable_tempo_safety,
        transform_robustness,
        recommendation,
        production_changes: vec![
            "none: production analyzers and resolvers unchanged".into(),
            "Neural tempo candidates change labels in this report only".into(),
            "Classical beat events, grid phase, meter, and downbeats remain authoritative".into(),
            "external product analysis is not an advisor input".into(),
        ],
    };
    write_json(&output_dir.join("tempo-advisor-research.json"), &report)?;
    write_json(&output_dir.join("baseline-classical.json"), &classical)?;
    write_json(&output_dir.join("baseline-hybrid.json"), &hybrid)?;
    write_json(
        &output_dir.join("refinement-comparison.json"),
        &serde_json::json!({
            "activation": report.activation_refined,
            "event": report.event_refined,
            "consensus": report.consensus_refined,
            "oracle": report.best_refined_oracle,
        }),
    )?;
    write_json(&output_dir.join("advisor-oof.json"), &report.nested_oof)?;
    write_json(
        &output_dir.join("advisor-family-stress.json"),
        &report.component_expanded_family_stress,
    )?;
    fs::write(
        output_dir.join("tempo-advisor-research.md"),
        tempo_advisor_markdown(&report),
    )?;
    fs::write(
        output_dir.join("README.txt"),
        "Research-only output. Classical owns beats/grid/meter/downbeats; Neural candidates may change only a tempo label. OOF decisions are grouped by connected PCM+lineage components. Full-data refit is not held-out evidence.\n",
    )?;
    Ok(report)
}

fn build_tempo_advisor_rows(
    blackbox: &BlackboxManifest,
    hybrid: &EvaluationReport,
    classical: &EvaluationReport,
) -> Result<Vec<TempoAdvisorRow>, LabError> {
    let hybrid_by_id = hybrid
        .per_track
        .iter()
        .map(|track| (track.sample_id.as_str(), track))
        .collect::<BTreeMap<_, _>>();
    let classical_by_id = classical
        .per_track
        .iter()
        .map(|track| (track.sample_id.as_str(), track))
        .collect::<BTreeMap<_, _>>();
    let experiment_by_id = hybrid
        .tempo_experiment
        .per_fixture
        .iter()
        .map(|fixture| (fixture.sample_id.as_str(), fixture))
        .collect::<BTreeMap<_, _>>();
    let pcm_by_id = blackbox
        .fixtures
        .iter()
        .map(|fixture| (fixture.sample_id.as_str(), fixture.pcm_sha256.clone()))
        .collect::<BTreeMap<_, _>>();
    let lineage = lineage_group_map(blackbox);
    let mut rows = Vec::new();
    for record in &blackbox.fixtures {
        let Some(truth) = record.truth.tempo.as_ref() else {
            continue;
        };
        if !matches!(record.spec.tempo, TempoProfile::Constant { .. }) {
            continue;
        }
        let Some(hybrid_track) = hybrid_by_id.get(record.sample_id.as_str()) else {
            continue;
        };
        let Some(classical_track) = classical_by_id.get(record.sample_id.as_str()) else {
            continue;
        };
        let Some(experiment) = experiment_by_id.get(record.sample_id.as_str()) else {
            continue;
        };
        let native = hybrid_track.analysis_backend == "native_neural";
        let current_bpm = native
            .then(|| primary_tempo_bpm(&hybrid_track.wotoha))
            .flatten();
        let current_relation = current_bpm
            .map(|bpm| relation_to_truth(Some(bpm), truth.primary_bpm, false))
            .unwrap_or_else(|| "absent".into());
        let selection_relation = if native {
            production_selection_relation(hybrid_track)
        } else {
            "unknown".into()
        };
        let activation = native
            .then(|| {
                experiment
                    .raw_observations
                    .as_ref()
                    .and_then(|observations| {
                        fractional_tempo_refinement(observations, current_bpm, &selection_relation)
                    })
            })
            .flatten();
        let event = native
            .then(|| {
                experiment.raw_observations.as_ref().map(|observations| {
                    beat_event_interval_refinement(observations, &selection_relation)
                })
            })
            .flatten();
        let activation_bpm = activation.as_ref().and_then(|value| value.selected_bpm);
        let event_bpm = event.as_ref().and_then(|value| value.selected_bpm);
        let consensus_bpm = activation_bpm.zip(event_bpm).and_then(|(left, right)| {
            ((left - right).abs() / left.max(1.0) <= 0.005).then_some((left + right) / 2.0)
        });
        let consensus_relation = consensus_bpm
            .map(|bpm| relation_to_truth(Some(bpm), truth.primary_bpm, false))
            .unwrap_or_else(|| "absent".into());
        let classical_bpm = primary_tempo_bpm(&classical_track.wotoha);
        let classical_relation = classical_bpm
            .map(|bpm| relation_to_truth(Some(bpm), truth.primary_bpm, false))
            .unwrap_or_else(|| "absent".into());
        let (candidate_label, candidate_bpm) =
            choose_refined_candidate(current_bpm, activation.as_ref(), event.as_ref());
        let (features, diagnostic_score) =
            advisor_features(hybrid_track, activation.as_ref(), event.as_ref());
        let fixture = TempoAdvisorFixture {
            sample_id: record.sample_id.clone(),
            family: record.spec.family.as_str().into(),
            truth_bpm: truth.primary_bpm,
            classical_bpm,
            classical_relation,
            current_neural_bpm: current_bpm,
            current_neural_relation: current_relation,
            activation_refined_bpm: activation_bpm,
            activation_refined_relation: activation_bpm
                .map(|bpm| relation_to_truth(Some(bpm), truth.primary_bpm, false))
                .unwrap_or_else(|| "absent".into()),
            event_refined_bpm: event_bpm,
            event_refined_relation: event_bpm
                .map(|bpm| relation_to_truth(Some(bpm), truth.primary_bpm, false))
                .unwrap_or_else(|| "absent".into()),
            consensus_bpm,
            consensus_relation,
            oracle_choice: "not_computed".into(),
            oof_advisor_choice: "Classical".into(),
            oof_advisor_selected_bpm: classical_bpm,
            oof_advisor_correct: None,
            advisor_features: features,
            outer_fold: None,
            event_refinement: event,
            neural_diagnostics: hybrid_track.neural_diagnostics.clone(),
            classical_tempo_hypotheses: classical_track.wotoha.tempo_hypotheses.clone(),
        };
        rows.push(TempoAdvisorRow {
            fixture,
            pcm_group: pcm_by_id
                .get(record.sample_id.as_str())
                .cloned()
                .unwrap_or_else(|| record.sample_id.clone()),
            lineage_group: lineage
                .get(&record.sample_id)
                .cloned()
                .unwrap_or_else(|| record.sample_id.clone()),
            leakage_group: String::new(),
            candidate_label,
            candidate_bpm,
            diagnostic_score,
        });
    }
    rows.sort_by(|left, right| left.fixture.sample_id.cmp(&right.fixture.sample_id));
    Ok(rows)
}

fn tempo_oracle_choice(row: &TempoAdvisorRow) -> String {
    let classical = row.fixture.classical_bpm;
    let (candidate_label, candidate) = best_research_candidate(row);
    match (classical, candidate) {
        (Some(classical), Some(candidate)) => {
            let classical_correct = canonical_tempo(Some(classical), row.fixture.truth_bpm);
            let candidate_correct = canonical_tempo(Some(candidate), row.fixture.truth_bpm);
            if candidate_correct && !classical_correct {
                candidate_label
            } else if classical_correct && !candidate_correct {
                "Classical".into()
            } else if (candidate - row.fixture.truth_bpm).abs()
                < (classical - row.fixture.truth_bpm).abs()
            {
                candidate_label
            } else {
                "Classical".into()
            }
        }
        (Some(_), None) => "Classical".into(),
        (None, Some(_)) => candidate_label,
        (None, None) => "unavailable".into(),
    }
}

/// Select the most accurate available research candidate for retrospective
/// oracle analysis only. This function is never used by the advisor inference
/// path and therefore must not be interpreted as a deployable decision rule.
fn best_research_candidate(row: &TempoAdvisorRow) -> (String, Option<f32>) {
    let mut candidates = vec![
        ("current_neural".to_owned(), row.fixture.current_neural_bpm),
        (
            "activation_refined".to_owned(),
            row.fixture.activation_refined_bpm,
        ),
        ("event_refined".to_owned(), row.fixture.event_refined_bpm),
        ("consensus_refined".to_owned(), row.fixture.consensus_bpm),
    ];
    candidates.retain(|(_, bpm)| bpm.is_some());
    candidates.sort_by(|left, right| {
        let left_bpm = left.1.expect("retained research candidates have BPM");
        let right_bpm = right.1.expect("retained research candidates have BPM");
        canonical_tempo(Some(right_bpm), row.fixture.truth_bpm)
            .cmp(&canonical_tempo(Some(left_bpm), row.fixture.truth_bpm))
            .then_with(|| {
                (left_bpm - row.fixture.truth_bpm)
                    .abs()
                    .total_cmp(&(right_bpm - row.fixture.truth_bpm).abs())
            })
            .then_with(|| left.0.cmp(&right.0))
    });
    candidates
        .into_iter()
        .next()
        .unwrap_or_else(|| ("unavailable".to_owned(), None))
}

fn tempo_advisor_oracle(
    rows: &[TempoAdvisorRow],
    candidate: impl Fn(&TempoAdvisorRow) -> Option<f32>,
) -> TempoAdvisorOracleComparison {
    let comparable = rows
        .iter()
        .filter(|row| candidate(row).is_some())
        .collect::<Vec<_>>();
    let classical_values = comparable
        .iter()
        .map(|row| {
            (
                row.fixture.sample_id.clone(),
                row.fixture.classical_bpm,
                row.fixture.truth_bpm,
            )
        })
        .collect::<Vec<_>>();
    let candidate_values = comparable
        .iter()
        .map(|row| {
            (
                row.fixture.sample_id.clone(),
                candidate(row),
                row.fixture.truth_bpm,
            )
        })
        .collect::<Vec<_>>();
    let mut oracle_values = Vec::new();
    let mut classical_rescues = Vec::new();
    let mut candidate_rescues = Vec::new();
    let mut both_correct = Vec::new();
    let mut both_wrong = Vec::new();
    for row in comparable {
        let classical = row.fixture.classical_bpm;
        let neural = candidate(row);
        let classical_correct = canonical_tempo(classical, row.fixture.truth_bpm);
        let neural_correct = canonical_tempo(neural, row.fixture.truth_bpm);
        if classical_correct && !neural_correct {
            classical_rescues.push(row.fixture.sample_id.clone());
        } else if !classical_correct && neural_correct {
            candidate_rescues.push(row.fixture.sample_id.clone());
        } else if classical_correct && neural_correct {
            both_correct.push(row.fixture.sample_id.clone());
        } else {
            both_wrong.push(row.fixture.sample_id.clone());
        }
        let selected = match (classical, neural) {
            (Some(_classical), Some(neural)) if neural_correct && !classical_correct => {
                Some(neural)
            }
            (Some(classical), Some(_neural)) if classical_correct && !neural_correct => {
                Some(classical)
            }
            (Some(classical), Some(neural)) => {
                let classical_error = (classical - row.fixture.truth_bpm).abs();
                let neural_error = (neural - row.fixture.truth_bpm).abs();
                Some(if neural_error < classical_error {
                    neural
                } else {
                    classical
                })
            }
            (Some(classical), None) => Some(classical),
            (None, Some(neural)) => Some(neural),
            (None, None) => None,
        };
        oracle_values.push((
            row.fixture.sample_id.clone(),
            selected,
            row.fixture.truth_bpm,
        ));
    }
    let classical = tempo_metric_summary(&classical_values);
    let candidate_summary = tempo_metric_summary(&candidate_values);
    let oracle = tempo_metric_summary(&oracle_values);
    TempoAdvisorOracleComparison {
        oracle_uplift_over_classical: oracle.canonical_correct as i64
            - classical.canonical_correct as i64,
        classical,
        candidate: candidate_summary,
        oracle,
        classical_rescues_candidate_cannot_provide: classical_rescues,
        candidate_rescues_classical: candidate_rescues,
        both_correct,
        both_wrong,
    }
}

fn select_tempo_advisor_threshold(rows: &[TempoAdvisorRow]) -> f64 {
    let mut thresholds = vec![1.000001_f64];
    thresholds.extend(
        rows.iter()
            .filter(|row| row.candidate_bpm.is_some())
            .map(|row| row.diagnostic_score),
    );
    thresholds.sort_by(f64::total_cmp);
    thresholds.dedup_by(|left, right| (*left - *right).abs() < f64::EPSILON);
    let mut best = (usize::MAX, usize::MAX, usize::MAX, f64::INFINITY);
    for threshold in thresholds {
        let mut false_accepts = 0;
        let mut rescues = 0;
        let mut precision_wins = 0;
        let mut coverage = 0;
        for row in rows {
            let accepts = row.candidate_bpm.is_some() && row.diagnostic_score >= threshold;
            if !accepts {
                continue;
            }
            coverage += 1;
            let classical_correct =
                canonical_tempo(row.fixture.classical_bpm, row.fixture.truth_bpm);
            let candidate_correct = canonical_tempo(row.candidate_bpm, row.fixture.truth_bpm);
            false_accepts += usize::from(classical_correct && !candidate_correct);
            rescues += usize::from(!classical_correct && candidate_correct);
            precision_wins += usize::from(
                classical_correct
                    && candidate_correct
                    && row
                        .candidate_bpm
                        .zip(row.fixture.classical_bpm)
                        .is_some_and(|(candidate, classical)| {
                            (candidate - row.fixture.truth_bpm).abs()
                                < (classical - row.fixture.truth_bpm).abs()
                        }),
            );
        }
        let key = (
            false_accepts,
            usize::MAX - rescues,
            usize::MAX - precision_wins,
            threshold,
        );
        if key < best {
            best = key;
            let _ = coverage;
        }
    }
    best.3
}

fn nested_tempo_advisor_threshold(rows: &[TempoAdvisorRow]) -> f64 {
    if rows.len() < 3 {
        return select_tempo_advisor_threshold(rows);
    }
    let groups = rows
        .iter()
        .map(|row| row.leakage_group.clone())
        .collect::<BTreeSet<_>>();
    if groups.len() < 2 {
        return select_tempo_advisor_threshold(rows);
    }
    let mut thresholds = Vec::new();
    for validation_group in groups {
        let inner_train = rows
            .iter()
            .filter(|row| row.leakage_group != validation_group)
            .cloned()
            .collect::<Vec<_>>();
        if !inner_train.is_empty() {
            thresholds.push(select_tempo_advisor_threshold(&inner_train));
        }
    }
    median_f64(&thresholds).unwrap_or_else(|| select_tempo_advisor_threshold(rows))
}

fn selected_tempo_value(row: &TempoAdvisorRow, accept: bool) -> (String, Option<f32>) {
    if accept && row.candidate_bpm.is_some() {
        (row.candidate_label.clone(), row.candidate_bpm)
    } else {
        ("Classical".into(), row.fixture.classical_bpm)
    }
}

fn run_tempo_advisor_oof(rows: &mut [TempoAdvisorRow]) -> TempoAdvisorNestedOof {
    let mut groups = BTreeMap::<String, Vec<usize>>::new();
    for (index, row) in rows.iter().enumerate() {
        groups
            .entry(row.leakage_group.clone())
            .or_default()
            .push(index);
    }
    let all_indices = (0..rows.len()).collect::<Vec<_>>();
    let mut decisions = BTreeMap::<String, (usize, String, Option<f32>)>::new();
    let mut audits = Vec::new();
    for (fold_index, (group, validation_indices)) in groups.iter().enumerate() {
        let training_indices = all_indices
            .iter()
            .copied()
            .filter(|index| !validation_indices.contains(index))
            .collect::<Vec<_>>();
        let training_rows = training_indices
            .iter()
            .map(|index| rows[*index].clone())
            .collect::<Vec<_>>();
        let threshold = nested_tempo_advisor_threshold(&training_rows);
        let validation_rows = validation_indices
            .iter()
            .map(|index| &rows[*index])
            .collect::<Vec<_>>();
        for row in &validation_rows {
            let accept = row.candidate_bpm.is_some() && row.diagnostic_score >= threshold;
            let (choice, bpm) = selected_tempo_value(row, accept);
            decisions.insert(row.fixture.sample_id.clone(), (fold_index, choice, bpm));
        }
        let train_pcm = training_indices
            .iter()
            .map(|index| rows[*index].pcm_group.clone())
            .collect::<BTreeSet<_>>();
        let validation_pcm = validation_indices
            .iter()
            .map(|index| rows[*index].pcm_group.clone())
            .collect::<BTreeSet<_>>();
        let train_lineage = training_indices
            .iter()
            .map(|index| rows[*index].lineage_group.clone())
            .collect::<BTreeSet<_>>();
        let validation_lineage = validation_indices
            .iter()
            .map(|index| rows[*index].lineage_group.clone())
            .collect::<BTreeSet<_>>();
        audits.push(TempoAdvisorFoldAudit {
            fold: fold_index,
            validation_sample_ids: validation_indices
                .iter()
                .map(|index| rows[*index].fixture.sample_id.clone())
                .collect(),
            training_sample_ids: training_indices
                .iter()
                .map(|index| rows[*index].fixture.sample_id.clone())
                .collect(),
            validation_groups: vec![group.clone()],
            training_groups: training_indices
                .iter()
                .map(|index| rows[*index].leakage_group.clone())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
            selected_threshold: threshold,
            selected_candidate: "frozen-small-interpretable-rule".into(),
            exact_pcm_overlap: !train_pcm.is_disjoint(&validation_pcm),
            lineage_overlap: !train_lineage.is_disjoint(&validation_lineage),
        });
    }
    let mut values = Vec::new();
    let mut false_accepts = Vec::new();
    let mut rescues = Vec::new();
    let mut coverage = 0;
    let mut abstentions = 0;
    for row in rows.iter_mut() {
        let (fold, choice, bpm) = decisions.get(&row.fixture.sample_id).cloned().unwrap_or((
            usize::MAX,
            "Classical".into(),
            row.fixture.classical_bpm,
        ));
        let accepts = choice != "Classical";
        coverage += usize::from(accepts);
        abstentions += usize::from(!accepts && row.candidate_bpm.is_some());
        let candidate_correct = canonical_tempo(bpm, row.fixture.truth_bpm);
        let classical_correct = canonical_tempo(row.fixture.classical_bpm, row.fixture.truth_bpm);
        if accepts && classical_correct && !candidate_correct {
            false_accepts.push(row.fixture.sample_id.clone());
        }
        if accepts && !classical_correct && candidate_correct {
            rescues.push(row.fixture.sample_id.clone());
        }
        row.fixture.outer_fold = (fold != usize::MAX).then_some(fold);
        row.fixture.oof_advisor_choice = choice;
        row.fixture.oof_advisor_selected_bpm = bpm;
        row.fixture.oof_advisor_correct = Some(candidate_correct);
        values.push((row.fixture.sample_id.clone(), bpm, row.fixture.truth_bpm));
    }
    let full_data_threshold = select_tempo_advisor_threshold(rows);
    TempoAdvisorNestedOof {
        grouping_rule:
            "outer leave-one-connected-PCM+lineage-component-out; inner grouped threshold selection"
                .into(),
        feature_set_frozen_before_oof: true,
        outer_fold_count: audits.len(),
        outer_folds: audits,
        advisor_metrics: tempo_metric_summary(&values),
        false_accept_sample_ids: false_accepts,
        canonical_rescue_sample_ids: rescues,
        neural_coverage: coverage,
        abstention_count: abstentions,
        full_data_refit_threshold: full_data_threshold,
        full_data_refit_is_not_held_out: true,
    }
}

fn build_tempo_advisor_family_stress(
    rows: &[TempoAdvisorRow],
    gate_rows: &[GateFeatureRow],
) -> Vec<TempoAdvisorFamilyStress> {
    let folds = component_expanded_leave_family_out_folds(gate_rows);
    folds
        .into_iter()
        .map(|fold| {
            let validation_ids = fold
                .validation_split
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>();
            let train_rows = rows
                .iter()
                .filter(|row| !validation_ids.contains(&row.fixture.sample_id))
                .cloned()
                .collect::<Vec<_>>();
            let validation_rows = rows
                .iter()
                .filter(|row| validation_ids.contains(&row.fixture.sample_id))
                .collect::<Vec<_>>();
            let threshold = nested_tempo_advisor_threshold(&train_rows);
            let values = validation_rows
                .iter()
                .map(|row| {
                    let accept = row.candidate_bpm.is_some() && row.diagnostic_score >= threshold;
                    let (_, bpm) = selected_tempo_value(row, accept);
                    (row.fixture.sample_id.clone(), bpm, row.fixture.truth_bpm)
                })
                .collect::<Vec<_>>();
            let requested = fold
                .validation_groups
                .first()
                .cloned()
                .unwrap_or_else(|| "unknown".into());
            let other_families = validation_rows
                .iter()
                .filter(|row| row.fixture.family != requested)
                .map(|row| row.fixture.family.clone())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            TempoAdvisorFamilyStress {
                requested_family: requested,
                expanded_validation_samples: fold.validation_split,
                other_families_pulled_into_validation: other_families,
                train_size: train_rows.len(),
                validation_size: validation_rows.len(),
                exact_pcm_overlap: fold.exact_pcm_overlap,
                lineage_overlap: fold.lineage_overlap,
                advisor_metrics: tempo_metric_summary(&values),
            }
        })
        .collect()
}

fn build_tempo_advisor_feature_inventory(
    rows: &[TempoAdvisorRow],
) -> Vec<TempoAdvisorFeatureDescription> {
    let definitions = [
        (
            "path_coverage",
            "fraction of activation path supported by selected model frames",
            "NeuralRhythmDiagnostics.path_coverage",
            "0.0 when unavailable",
            "lab diagnostic; derivable at analysis time",
        ),
        (
            "support",
            "activation support for the decoded path",
            "NeuralRhythmDiagnostics.support",
            "0.0 when unavailable",
            "lab diagnostic; derivable at analysis time",
        ),
        (
            "interval_residual",
            "normalized residual of the selected Neural event path",
            "NeuralRhythmDiagnostics.interval_residual",
            "1.0 when unavailable",
            "lab diagnostic; derivable at analysis time",
        ),
        (
            "alias_margin",
            "margin against the nearest tempo-family alias",
            "NeuralRhythmDiagnostics.alias_margin",
            "0.0 when unavailable",
            "lab diagnostic; derivable at analysis time",
        ),
        (
            "activation_refinement_quality",
            "bounded score from fractional activation support, coverage, periodicity, and phase stability",
            "FractionalTempoRefinement",
            "0.0 when unavailable",
            "research-only diagnostic",
        ),
        (
            "event_refinement_quality",
            "bounded score from robust BeatEvent interval dispersion, residual, and drift",
            "BeatEventIntervalRefinement",
            "0.0 when unavailable",
            "research-only diagnostic",
        ),
        (
            "activation_event_disagreement",
            "relative BPM disagreement between independent Neural refinements",
            "activation and event candidates",
            "1.0 unless both are available",
            "research-only diagnostic",
        ),
        (
            "classical_tempo_margin",
            "margin between Classical tempo hypothesis weights",
            "Classical NormalizedAnalysis.tempo_hypotheses",
            "0.0 when unavailable",
            "lab diagnostic; not Ground Truth",
        ),
        (
            "neural_path_marker_count",
            "number of events in the accepted Neural path",
            "NeuralRhythmDiagnostics.path_marker_count",
            "0 when unavailable",
            "lab diagnostic; derivable at analysis time",
        ),
    ];
    definitions
        .into_iter()
        .map(|(name, definition, source, missing, exposure)| {
            let values = rows
                .iter()
                .filter_map(|row| row.fixture.advisor_features.get(name).copied())
                .collect::<Vec<_>>();
            TempoAdvisorFeatureDescription {
                name: name.into(),
                definition: definition.into(),
                source: source.into(),
                production_time_available: true,
                missing_value_behavior: missing.into(),
                observed_min: values.iter().copied().reduce(f64::min),
                observed_median: median_f64(&values),
                observed_max: values.iter().copied().reduce(f64::max),
                exposure: exposure.into(),
            }
        })
        .collect()
}

fn build_variable_tempo_safety(
    _rows: &[TempoAdvisorRow],
    hybrid: &EvaluationReport,
) -> Vec<TempoAdvisorVariableTempoSafety> {
    hybrid
        .per_track
        .iter()
        .filter(|track| track.family == FixtureFamily::TempoDrift.as_str())
        .filter_map(|track| {
            let raw = hybrid.research_raw_observations.get(&track.sample_id)?;
            let refinement = beat_event_interval_refinement(raw, "primary");
            Some(TempoAdvisorVariableTempoSafety {
                sample_id: track.sample_id.clone(),
                family: track.family.clone(),
                fit_residual_micros: refinement.fit_residual_micros,
                interval_dispersion: refinement.relative_dispersion,
                early_period_micros: refinement
                    .selected_period_micros
                    .zip(refinement.early_late_drift)
                    .map(|(period, drift)| period * (1.0 - drift)),
                middle_period_micros: refinement.middle_period_micros,
                late_period_micros: refinement
                    .selected_period_micros
                    .zip(refinement.early_late_drift)
                    .map(|(period, drift)| period * (1.0 + drift)),
                refinement_available: refinement.available,
                abstention_reason: refinement.reason,
            })
        })
        .collect()
}

fn build_tempo_transform_robustness(
    blackbox: &BlackboxManifest,
    rows: &[TempoAdvisorRow],
) -> BTreeMap<String, TempoAdvisorTransformPair> {
    let by_id = rows
        .iter()
        .map(|row| (row.fixture.sample_id.as_str(), row))
        .collect::<BTreeMap<_, _>>();
    let mut result = BTreeMap::new();
    for fixture in &blackbox.fixtures {
        let Some(base_id) = fixture.spec.base_id.as_deref() else {
            continue;
        };
        let (Some(base), Some(transformed)) =
            (by_id.get(base_id), by_id.get(fixture.sample_id.as_str()))
        else {
            continue;
        };
        let key = fixture.spec.transform.label().to_owned();
        result
            .entry(key)
            .or_insert_with(|| TempoAdvisorTransformPair {
                base_sample_id: base.fixture.sample_id.clone(),
                transformed_sample_id: transformed.fixture.sample_id.clone(),
                activation_bpm_delta: base
                    .fixture
                    .activation_refined_bpm
                    .zip(transformed.fixture.activation_refined_bpm)
                    .map(|(before, after)| after - before),
                event_bpm_delta: base
                    .fixture
                    .event_refined_bpm
                    .zip(transformed.fixture.event_refined_bpm)
                    .map(|(before, after)| after - before),
                advisor_decision_changed: base.fixture.oof_advisor_choice
                    != transformed.fixture.oof_advisor_choice,
            });
    }
    result
}

fn tempo_advisor_markdown(report: &TempoAdvisorReport) -> String {
    let mut markdown = format!(
        "# Classical rhythm + Neural tempo advisor research\n\nSource commit: `{}`\nStarting commit: `{}`\nCorpus: {} fixtures; {} scalar-tempo fixtures; seed `{}`\n\n## Executive summary\n\n{}\n\n",
        report.source_commit,
        report.starting_commit.as_deref().unwrap_or("unknown"),
        report.fixture_count,
        report.scalar_tempo_fixture_count,
        report.corpus_seed,
        report.recommendation,
    );
    markdown.push_str(
        "The hypothetical architecture fixes Classical beats, grid phase, meter, and downbeats. Neural candidates can change only a research tempo label. External product analysis is not an advisor input.\n\n",
    );
    markdown.push_str("## Baseline and refinement comparison\n\n| Candidate | canonical | half/double/other wrong | absolute BPM MAE/median/p95 |\n|---|---:|---:|---:|\n");
    for (name, metric) in [
        ("Always Classical", &report.baseline_classical),
        ("Current Neural", &report.baseline_current_neural),
        ("Activation refined", &report.activation_refined),
        ("Event refined", &report.event_refined),
        ("Consensus refined", &report.consensus_refined),
        ("Best refined candidate", &report.refined_oracle),
        ("Nested OOF advisor", &report.nested_oof.advisor_metrics),
    ] {
        markdown.push_str(&format!(
            "| {} | {}/{} ({:?}) | {}/{}/{} | {:?}/{:?}/{:?} |\n",
            name,
            metric.canonical_correct,
            metric.scored,
            metric.canonical_accuracy,
            metric.half_time,
            metric.double_time,
            metric.other_wrong,
            metric.absolute_bpm_mae,
            metric.absolute_bpm_median,
            metric.absolute_bpm_p95,
        ));
    }
    markdown.push_str("\n## Refined-tempo oracle upper bounds\n\n");
    for (name, oracle) in [
        ("activation", &report.activation_oracle),
        ("event", &report.event_oracle),
        ("best refined", &report.best_refined_oracle),
    ] {
        markdown.push_str(&format!(
            "- {}: uplift={} tracks; candidate rescues={:?}; Classical rescues={:?}.\n",
            name,
            oracle.oracle_uplift_over_classical,
            oracle.candidate_rescues_classical,
            oracle.classical_rescues_candidate_cannot_provide,
        ));
    }
    markdown.push_str("\nThese are Ground-Truth-selected upper bounds, not deployable advisor results.\n\n## Classical failure budget\n\n| sample | family | Classical | current Neural | activation | event | oracle |\n|---|---|---:|---:|---:|---:|---|\n");
    for row in &report.classical_failure_budget {
        markdown.push_str(&format!(
            "| {} | {} | {:?} ({}) | {:?} ({}) | {:?} ({}) | {:?} ({}) | {} |\n",
            row.sample_id,
            row.family,
            row.classical_bpm,
            row.classical_relation,
            row.current_neural_bpm,
            row.current_neural_relation,
            row.activation_refined_bpm,
            row.activation_refined_relation,
            row.event_refined_bpm,
            row.event_refined_relation,
            row.oracle_choice,
        ));
    }
    markdown.push_str("\n## Allowed feature inventory\n\n| feature | definition | source | min/median/max | missing behavior |\n|---|---|---|---:|---|\n");
    for feature in &report.feature_inventory {
        markdown.push_str(&format!(
            "| {} | {} | {} | {:?}/{:?}/{:?} | {} |\n",
            feature.name,
            feature.definition,
            feature.source,
            feature.observed_min,
            feature.observed_median,
            feature.observed_max,
            feature.missing_value_behavior,
        ));
    }
    markdown.push_str(&format!(
        "\nThe feature set was frozen before final outer OOF. It excludes truth, IDs, family, hashes, lineage, transform identity, and vendor-specific external analysis.\n\n## Nested grouped OOF advisor\n\nGrouping: `{}`\nOuter folds: {}\nExact PCM overlap: {}\nLineage overlap: {}\nNeural coverage: {}\nAbstentions: {}\nFalse accepts: {} ({:?})\nCanonical rescues: {} ({:?})\nFull-data refit threshold: {:.6}; full-data refit is not held-out evidence.\n\n",
        report.nested_oof.grouping_rule,
        report.nested_oof.outer_fold_count,
        report.nested_oof.outer_folds.iter().any(|fold| fold.exact_pcm_overlap),
        report.nested_oof.outer_folds.iter().any(|fold| fold.lineage_overlap),
        report.nested_oof.neural_coverage,
        report.nested_oof.abstention_count,
        report.nested_oof.false_accept_sample_ids.len(),
        report.nested_oof.false_accept_sample_ids,
        report.nested_oof.canonical_rescue_sample_ids.len(),
        report.nested_oof.canonical_rescue_sample_ids,
        report.nested_oof.full_data_refit_threshold,
    ));
    markdown.push_str("## Component-expanded leave-family-out stress\n\n| requested family | validation size | other families pulled in | correct/scored | PCM overlap/lineage overlap |\n|---|---:|---|---:|---:|\n");
    for family in &report.component_expanded_family_stress {
        markdown.push_str(&format!(
            "| {} | {} | {} | {}/{} | {}/{} |\n",
            family.requested_family,
            family.validation_size,
            family.other_families_pulled_into_validation.join(", "),
            family.advisor_metrics.canonical_correct,
            family.advisor_metrics.scored,
            family.exact_pcm_overlap,
            family.lineage_overlap,
        ));
    }
    markdown.push_str("\n## Variable-tempo safety\n\nScalar-tempo metrics exclude ramps and step-return fixtures.\n\n| sample | early | middle | late | dispersion | available | reason |\n|---|---:|---:|---:|---:|---:|---|\n");
    for item in &report.variable_tempo_safety {
        markdown.push_str(&format!(
            "| {} | {:?} | {:?} | {:?} | {:?} | {} | {:?} |\n",
            item.sample_id,
            item.early_period_micros,
            item.middle_period_micros,
            item.late_period_micros,
            item.interval_dispersion,
            item.refinement_available,
            item.abstention_reason,
        ));
    }
    markdown.push_str("\n## Transform robustness\n\n");
    for (name, pair) in &report.transform_robustness {
        markdown.push_str(&format!(
            "- `{}`: {} → {}; activation Δ={:?}, event Δ={:?}, advisor decision changed={}.\n",
            name,
            pair.base_sample_id,
            pair.transformed_sample_id,
            pair.activation_bpm_delta,
            pair.event_bpm_delta,
            pair.advisor_decision_changed,
        ));
    }
    markdown.push_str("\n## Recommendation\n\n");
    markdown.push_str(&report.recommendation);
    markdown.push_str("\n\n## Production freeze\n\nProduction source behavior, Classical/Neural analyzers, BeatEvent timestamps, tempo resolver, grid, meter, downbeat, AutoMix, runtime, Discord, yt-dlp, and deployment are unchanged. Generated artifacts remain outside Git.\n");
    markdown
}

fn research_summary_markdown_corrected(
    summary: &ResearchSummary,
    oracle: &BackendOracleReport,
    gate: &GateResearchReport,
    tempo: &TempoRefinementReport,
    meter: &MeterResearchReport,
) -> String {
    let leave_family_out = gate
        .leave_family_out_folds
        .iter()
        .map(|fold| {
            format!(
                "{}: train={} validation={} threshold={:.4} false_accepts={} false_rejects={}",
                fold.validation_groups.join(","),
                fold.train_split.len(),
                fold.validation_split.len(),
                fold.threshold,
                fold.false_accept_bad_neural_count,
                fold.false_reject_useful_neural_count,
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    let named_tempo_cases = tempo
        .required_cases
        .iter()
        .map(|(sample_id, case)| {
            format!(
                "{}: backend={} primary={} start={:?} refined={:?} selection={}->{} truth={}->{}",
                sample_id,
                case.current_backend,
                case.primary_cohort,
                case.current_selected_bpm,
                case.refined_bpm,
                case.selection_relation_before,
                case.selection_relation_after,
                case.truth_relation_before,
                case.truth_relation_after,
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let ambiguous_meter_cases = meter
        .ambiguous_fixtures
        .iter()
        .map(|fixture| {
            format!(
                "{}: resolved={:?} reason={:?}",
                fixture.sample_id, fixture.experimental_resolved_meter, fixture.unknown_reason
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "# Wotoha ground-truth analyzer research\n\n\
Source commit: `{}`\nStarting commit: `{}`\nCorpus: {} exported WAV fixtures, seed `{}`\n\n\
## Decision\n\nAlways Classical is the first-class null hypothesis. Gate: **{}**. Tempo refinement: **{}**. Meter scorer: **{}**.\n\n\
## Current exported-WAV baselines\n\n\
| Backend | beat MAE ms | p50 ms | p95 ms | P@40 | R@40 | tempo | grid phase |\n|---|---:|---:|---:|---:|---:|---:|---:|\n\
| Classical | {:?} | {:?} | {:?} | {:?} | {:?} | {}/{} ({:?}) | {}/{} ({:?}) |\n\
| Hybrid | {:?} | {:?} | {:?} | {:?} | {:?} | {}/{} ({:?}) | {}/{} ({:?}) |\n\
| Always Neural when available | {:?} | {:?} | {:?} | {:?} | {:?} | {}/{} ({:?}) | {}/{} ({:?}) |\n\n\
Native neural count: {}; Classical fallback count: {}.\nBackend outcomes: {:?}\n\n\
## Independent backend oracles\n\n\
Beat oracle: available={} MAE/p50/p95={:?}/{:?}/{:?}, P@40/R@40={:?}/{:?}.\n\
Tempo oracle: {}/{} ({:?}).\n\
Grid-phase oracle: {}/{} ({:?}), mean error={:?} ms.\n\
Joint dominance: {:?}\nNeural-dominant fixtures: {:?}\nClassical-dominant fixtures: {:?}\n\n\
## Gate research (primary evidence is OOF)\n\n\
Features: {:?}\nPrimary grouping: {}\nConnected leakage groups: {} (largest {})\nDirect primary-fold overlap: exact PCM={} lineage={}\nOOF decisions: {} (one per fixture)\nOOF neural coverage: {}; Classical use: {}; native regressions/false accepts: {}/{}; useful-neural false rejects: {}\n\
OOF beat MAE/p50/p95: {:?}/{:?}/{:?}; P@40/R@40: {:?}/{:?}\nOOF tempo: {}/{} ({:?}); grid phase: {}/{} ({:?})\n\
Full-data refit threshold: {:?} ({:?}); its metrics are not held-out evidence.\n\
Leave-family-out: {}\nRecommendation: {}\n\n\
## Fractional neural tempo refinement\n\n\
Primary cohort: {} accepted native-neural scalar fixtures; fallback exclusions: {}.\n\
Current/refined BPM MAE: {:?}/{:?}; median: {:?}/{:?}; p95: {:?}/{:?}.\n\
Canonical correctness: {}/{}; truth half-time: {}/{}; double-time: {}/{}; other-wrong: {}/{}.\n\
Selection relation is preserved by construction; truth relation is evaluated independently.\nRequired cases:\n{}\nRecommendation: {}\n\n\
## Contrastive meter × phase\n\n\
Primary set: {} clear Meter fixtures and {} ambiguous Meter fixtures.\n\
Current clear correct/unknown/wrong: {}/{}/{}\nExperimental clear correct/unknown/wrong: {}/{}/{}\nAmbiguous resolved/unknown: {}/{}\n\
Experimental non-4 recovery: {} ({:?})\nCurrent non-4 failures: {:?}\nInterpretation: {}\nAmbiguous cases:\n{}\nRecommendation: {}\n\n\
## Old experiments and scope\n\n\
PCM resolver primary correct: {}; activation resolver primary correct: {}. Diagnostic only; neither is promoted.\n\n{}\n",
        summary.source_commit.as_deref().unwrap_or("null"),
        summary.starting_commit.as_deref().unwrap_or("unknown"),
        summary.fixture_count,
        summary.corpus_seed,
        gate.production_recommendation,
        tempo.production_recommendation,
        meter.production_recommendation,
        summary.current_classical.beat_mae_ms,
        summary.current_classical.beat_p50_ms,
        summary.current_classical.beat_p95_ms,
        summary.current_classical.precision_at_40ms,
        summary.current_classical.recall_at_40ms,
        summary.current_classical.tempo_correct,
        summary.current_classical.tempo_scored,
        summary.current_classical.tempo_accuracy,
        summary.current_classical.grid_phase_correct,
        summary.current_classical.grid_phase_scored,
        summary.current_classical.grid_phase_accuracy,
        summary.current_hybrid.beat_mae_ms,
        summary.current_hybrid.beat_p50_ms,
        summary.current_hybrid.beat_p95_ms,
        summary.current_hybrid.precision_at_40ms,
        summary.current_hybrid.recall_at_40ms,
        summary.current_hybrid.tempo_correct,
        summary.current_hybrid.tempo_scored,
        summary.current_hybrid.tempo_accuracy,
        summary.current_hybrid.grid_phase_correct,
        summary.current_hybrid.grid_phase_scored,
        summary.current_hybrid.grid_phase_accuracy,
        oracle.always_neural_where_available.beat_mae_ms,
        oracle.always_neural_where_available.beat_p50_ms,
        oracle.always_neural_where_available.beat_p95_ms,
        oracle.always_neural_where_available.precision_at_40ms,
        oracle.always_neural_where_available.recall_at_40ms,
        oracle.always_neural_where_available.tempo_correct,
        oracle.always_neural_where_available.tempo_scored,
        oracle.always_neural_where_available.tempo_accuracy,
        oracle.always_neural_where_available.grid_phase_correct,
        oracle.always_neural_where_available.grid_phase_scored,
        oracle.always_neural_where_available.grid_phase_accuracy,
        gate.current_hybrid_neural_coverage,
        summary.fixture_count - gate.current_hybrid_neural_coverage,
        summary.backend_outcome_counts,
        oracle.beat_oracle.available_tracks,
        oracle.beat_oracle.beat_mae_ms,
        oracle.beat_oracle.beat_p50_ms,
        oracle.beat_oracle.beat_p95_ms,
        oracle.beat_oracle.precision_at_40ms,
        oracle.beat_oracle.recall_at_40ms,
        oracle.tempo_oracle.correct_count,
        oracle.tempo_oracle.scored_tracks,
        oracle.tempo_oracle.accuracy,
        oracle.grid_phase_oracle.correct_count,
        oracle.grid_phase_oracle.scored_tracks,
        oracle.grid_phase_oracle.accuracy,
        oracle.grid_phase_oracle.mean_error_ms,
        oracle.joint_outcome_counts,
        oracle.neural_dominates,
        oracle.classical_dominates,
        gate.feature_names,
        gate.validation_grouping,
        gate.leakage_group_count,
        gate.largest_leakage_group_size,
        gate.exact_pcm_duplicate_leakage,
        gate.lineage_leakage,
        gate.oof_decisions.len(),
        gate.candidate_gate_neural_coverage,
        gate.candidate_gate_fallback_use_classical_count,
        gate.candidate_gate_native_regression_count,
        gate.oof_false_accept_bad_neural_count,
        gate.oof_false_reject_useful_neural_count,
        gate.candidate_gate.beat_mae_ms,
        gate.candidate_gate.beat_p50_ms,
        gate.candidate_gate.beat_p95_ms,
        gate.candidate_gate.precision_at_40ms,
        gate.candidate_gate.recall_at_40ms,
        gate.candidate_gate.tempo_correct,
        gate.candidate_gate.tempo_scored,
        gate.candidate_gate.tempo_accuracy,
        gate.candidate_gate.grid_phase_correct,
        gate.candidate_gate.grid_phase_scored,
        gate.candidate_gate.grid_phase_accuracy,
        gate.full_data_refit_threshold,
        gate.full_data_refit_threshold_reason,
        leave_family_out,
        gate.production_recommendation,
        tempo.primary_cohort_size,
        tempo.fallback_excluded_count,
        tempo.current_neural_mean_absolute_error_bpm,
        tempo.refined_mean_absolute_error_bpm,
        tempo.current_median_absolute_error_bpm,
        tempo.refined_median_absolute_error_bpm,
        tempo.current_p95_absolute_error_bpm,
        tempo.refined_p95_absolute_error_bpm,
        tempo.current_canonical_correctness,
        tempo.refined_canonical_correctness,
        tempo.current_half_time_count,
        tempo.refined_half_time_count,
        tempo.current_double_time_count,
        tempo.refined_double_time_count,
        tempo.current_other_wrong_count,
        tempo.refined_other_wrong_count,
        named_tempo_cases,
        tempo.production_recommendation,
        meter.clear_fixture_count,
        meter.ambiguous_fixture_count,
        meter.current_clear_correct,
        meter.current_clear_unknown,
        meter.current_clear_wrong,
        meter.experimental_clear_correct,
        meter.experimental_clear_unknown,
        meter.experimental_clear_wrong,
        meter.ambiguous_resolved_count,
        meter.ambiguous_unknown_count,
        meter.experimental_recovers_non4_meter_count,
        meter.experimental_recovers_non4_meter_fixtures,
        meter.current_non4_meter_failures,
        meter.four_phase_prior_interpretation,
        ambiguous_meter_cases,
        meter.production_recommendation,
        summary.old_pcm_experiment_primary_correct,
        summary.old_activation_experiment_primary_correct,
        summary
            .scope_guarantees
            .iter()
            .map(|value| format!("- {value}"))
            .collect::<Vec<_>>()
            .join("\n"),
    )
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
        scored_tracks: 1,
        unobserved_tracks: 0,
        mae_ms: mean(&errors),
        p50_ms: percentile(&errors, 0.50),
        p95_ms: percentile(&errors, 0.95),
        precision_at_tolerance: precision,
        recall_at_tolerance: recall,
        matched_errors_ms: errors,
        matched_by_tolerance,
    }
}

fn unobserved_beat_metrics() -> BeatMetrics {
    BeatMetrics {
        unobserved_tracks: 1,
        ..BeatMetrics::default()
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
    if truth.tempo.is_none() {
        return PhaseMetrics::default();
    }
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

fn robust_median_u64(values: &[u64]) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    Some(sorted[sorted.len() / 2])
}

/// Infer a constant period only from a sufficiently stable external beatgrid.
///
/// The median makes the estimate insensitive to one modest outlier. Requiring
/// every valid interval to remain within the generic three-percent stability
/// band prevents a drifting grid from being reduced to its first interval.
fn stable_external_period(beat_times: &[u64]) -> Option<u64> {
    let intervals = beat_times
        .windows(2)
        .filter_map(|window| {
            let interval = window[1].checked_sub(window[0])?;
            (interval > 0).then_some(interval)
        })
        .collect::<Vec<_>>();
    if intervals.len() < 2 {
        return None;
    }
    let median = robust_median_u64(&intervals)?;
    let median_f64 = median as f64;
    let stable = intervals.iter().all(|interval| {
        (*interval as f64 - median_f64).abs() / median_f64 <= EXTERNAL_PERIOD_STABILITY
    });
    stable.then_some(median)
}

fn bpm_period_micros(bpm: Option<f32>) -> Option<u64> {
    bpm.filter(|bpm| bpm.is_finite() && *bpm > 0.0)
        .map(|bpm| (60_000_000.0 / f64::from(bpm)).round() as u64)
        .filter(|period| *period > 0)
}

/// Return the period and phase reference that make a global Wotoha↔external
/// phase comparison legitimate. An explicit BPM+phase pair is sufficient
/// without a beatgrid; a supplied beatgrid must first pass stable-period and,
/// when present, reported-BPM compatibility checks.
fn external_phase_reference(
    beat_times: Option<&[u64]>,
    reported_bpm: Option<f32>,
    explicit_phase: Option<u64>,
) -> Option<(u64, u64)> {
    let reported_period = bpm_period_micros(reported_bpm);
    let (period, inferred_phase) = match beat_times {
        Some(beat_times) => {
            let stable_period = stable_external_period(beat_times)?;
            if let Some(reported_period) = reported_period {
                let deviation =
                    stable_period.abs_diff(reported_period) as f64 / reported_period.max(1) as f64;
                if deviation > EXTERNAL_PERIOD_STABILITY {
                    return None;
                }
                (reported_period, beat_times.first().copied())
            } else {
                (stable_period, beat_times.first().copied())
            }
        }
        None => (reported_period?, None),
    };
    let phase = explicit_phase.or(inferred_phase)?;
    Some((phase, period))
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
    aggregate.beat.scored_tracks = groups.iter().map(|group| group.beat.scored_tracks).sum();
    aggregate.beat.unobserved_tracks = groups
        .iter()
        .map(|group| group.beat.unobserved_tracks)
        .sum();
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
    backends: &BTreeMap<String, String>,
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
        let base_low_frequency = mean(
            &base
                .confidence
                .low_frequency
                .iter()
                .map(|value| f64::from(*value))
                .collect::<Vec<_>>(),
        );
        let transformed_low_frequency = mean(
            &transformed
                .confidence
                .low_frequency
                .iter()
                .map(|value| f64::from(*value))
                .collect::<Vec<_>>(),
        );
        let base_confidence = mean(
            &base
                .confidence
                .timing
                .iter()
                .map(|value| f64::from(*value))
                .collect::<Vec<_>>(),
        );
        let transformed_confidence = mean(
            &transformed
                .confidence
                .timing
                .iter()
                .map(|value| f64::from(*value))
                .collect::<Vec<_>>(),
        );
        let key = spec.spec.transform.label().to_owned();
        let entry = result
            .entry(key.clone())
            .or_insert_with(|| TransformMetrics {
                transform: key,
                expectation: spec.spec.transform.expectation(),
                ..TransformMetrics::default()
            });
        entry.tracks += 1;
        entry.base_beat_count += base.beats.len();
        entry.transformed_beat_count += transformed.beats.len();
        entry.matched_beats += pairs.len();
        entry.missing_beats += base.beats.len().saturating_sub(pairs.len());
        entry.extra_beats += transformed.beats.len().saturating_sub(pairs.len());
        entry.displacement_ms.extend(displacement.iter().copied());
        entry.mean_beat_displacement_ms = mean(&entry.displacement_ms);
        entry.p95_beat_displacement_ms = percentile(&entry.displacement_ms, 0.95);
        entry.tempo_before_bpm = entry.tempo_before_bpm.or_else(|| primary_tempo_bpm(base));
        entry.tempo_after_bpm = entry
            .tempo_after_bpm
            .or_else(|| primary_tempo_bpm(transformed));
        entry.backend_before = entry.backend_before.clone().or_else(|| {
            backends
                .get(base_id)
                .cloned()
                .or_else(|| Some(base.analyzer.clone()))
        });
        entry.backend_after = entry.backend_after.clone().or_else(|| {
            backends
                .get(&spec.spec.id)
                .cloned()
                .or_else(|| Some(transformed.analyzer.clone()))
        });
        entry.low_frequency_support_change = combine_means(
            entry.low_frequency_support_change,
            transformed_low_frequency
                .zip(base_low_frequency)
                .map(|(after, before)| after - before),
            entry.tracks,
        );
        entry.confidence_before =
            combine_means(entry.confidence_before, base_confidence, entry.tracks);
        entry.confidence_after =
            combine_means(entry.confidence_after, transformed_confidence, entry.tracks);
        entry.tempo_interpretation_changes += usize::from(tempo_changes);
        entry.downbeat_changes += usize::from(downbeat_changes);
        entry.meter_changes += usize::from(meter_changes);
        entry.confidence_change = combine_means(entry.confidence_change, confidence, entry.tracks);
    }
    result
}

fn backend_snapshot(
    truth: &AnalysisGroundTruth,
    prediction: &NormalizedAnalysis,
) -> BackendMetricSnapshot {
    let metrics = metrics_for(truth, prediction);
    let recall = metrics.beat.recall_at_tolerance.get("40ms").copied();
    let core_valid = recall.is_some_and(|value| value >= 0.5)
        && truth
            .tempo
            .as_ref()
            .is_none_or(|_| metrics.tempo.primary_correct == Some(true));
    BackendMetricSnapshot {
        beat_mae_ms: metrics.beat.mae_ms,
        beat_p50_ms: metrics.beat.p50_ms,
        beat_p95_ms: metrics.beat.p95_ms,
        precision_at_40ms: metrics.beat.precision_at_tolerance.get("40ms").copied(),
        recall_at_40ms: recall,
        tempo_absolute_error_bpm: metrics.tempo.absolute_error_bpm.map(f64::from),
        primary_tempo_correct: metrics.tempo.primary_correct,
        grid_phase_error_ms: metrics.grid_phase.error_ms,
        grid_phase_correct: metrics.grid_phase.correct,
        downbeat_status: metrics
            .downbeat
            .phase_correct
            .map(|value| value.to_string())
            .unwrap_or_else(|| "unscored".into()),
        meter_status: metrics.meter.status,
        core_valid,
    }
}

fn metric_delta(left: Option<f64>, right: Option<f64>) -> Option<f64> {
    left.zip(right).map(|(left, right)| left - right)
}

fn classify_backend_outcome(
    hybrid: &BackendMetricSnapshot,
    classical: &BackendMetricSnapshot,
    hybrid_backend: &str,
) -> &'static str {
    if !hybrid.core_valid && !classical.core_valid {
        return "both_failed";
    }
    if hybrid_backend == "classical_fallback" && classical.core_valid {
        return "fallback_classical_rescued";
    }
    let recall_delta = metric_delta(hybrid.recall_at_40ms, classical.recall_at_40ms).unwrap_or(0.0);
    let mae_delta = metric_delta(hybrid.beat_mae_ms, classical.beat_mae_ms).unwrap_or(0.0);
    let tempo_delta = match (
        hybrid.primary_tempo_correct,
        classical.primary_tempo_correct,
    ) {
        (Some(true), Some(false)) => 1,
        (Some(false), Some(true)) => -1,
        _ => 0,
    };
    if (recall_delta >= 0.05 && mae_delta <= 1.0) || tempo_delta > 0 {
        "native_neural_improved"
    } else if (recall_delta <= -0.05 && mae_delta >= -1.0) || tempo_delta < 0 {
        "native_neural_regressed"
    } else {
        "mixed_or_equal"
    }
}

fn backend_comparison(
    manifest: &SyntheticCorpusManifest,
    runs: &BTreeMap<String, BackendRunSet>,
) -> BackendComparisonReport {
    let mut report = BackendComparisonReport::default();
    for record in &manifest.fixtures {
        let Some(run) = runs.get(&record.spec.id) else {
            continue;
        };
        let hybrid = backend_snapshot(&record.truth, &run.hybrid);
        let classical = backend_snapshot(&record.truth, &run.classical);
        let outcome = classify_backend_outcome(&hybrid, &classical, &run.hybrid_backend);
        *report.outcome_counts.entry(outcome.into()).or_default() += 1;
        *report
            .by_fixture_family
            .entry(record.spec.family.as_str().into())
            .or_default()
            .entry(outcome.into())
            .or_default() += 1;
        report.per_fixture.push(BackendFixtureComparison {
            sample_id: record.spec.id.clone(),
            family: record.spec.family.as_str().into(),
            hybrid_backend: run.hybrid_backend.clone(),
            deltas: BackendMetricDelta {
                beat_mae_ms: metric_delta(hybrid.beat_mae_ms, classical.beat_mae_ms),
                beat_p95_ms: metric_delta(hybrid.beat_p95_ms, classical.beat_p95_ms),
                precision_at_40ms: metric_delta(
                    hybrid.precision_at_40ms,
                    classical.precision_at_40ms,
                ),
                recall_at_40ms: metric_delta(hybrid.recall_at_40ms, classical.recall_at_40ms),
            },
            hybrid,
            classical,
            hybrid_metrics: metrics_for(&record.truth, &run.hybrid),
            classical_metrics: metrics_for(&record.truth, &run.classical),
            outcome: outcome.into(),
        });
    }
    report
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BackendMaterialThresholds {
    pub beat_mae_ms: f64,
    pub beat_p95_ms: f64,
    pub precision_recall: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ResearchBackendSummary {
    pub available_tracks: usize,
    pub beat_mae_ms: Option<f64>,
    pub beat_p50_ms: Option<f64>,
    pub beat_p95_ms: Option<f64>,
    pub precision_at_tolerance: BTreeMap<String, f64>,
    pub recall_at_tolerance: BTreeMap<String, f64>,
    pub precision_at_40ms: Option<f64>,
    pub recall_at_40ms: Option<f64>,
    pub tempo_scored: usize,
    pub tempo_correct: usize,
    pub tempo_accuracy: Option<f64>,
    pub grid_phase_scored: usize,
    pub grid_phase_correct: usize,
    pub grid_phase_accuracy: Option<f64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BeatOracleSummary {
    pub available_tracks: usize,
    pub beat_mae_ms: Option<f64>,
    pub beat_p50_ms: Option<f64>,
    pub beat_p95_ms: Option<f64>,
    pub precision_at_tolerance: BTreeMap<String, f64>,
    pub recall_at_tolerance: BTreeMap<String, f64>,
    pub precision_at_40ms: Option<f64>,
    pub recall_at_40ms: Option<f64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TempoOracleSummary {
    pub scored_tracks: usize,
    pub correct_count: usize,
    pub accuracy: Option<f64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GridPhaseOracleSummary {
    pub scored_tracks: usize,
    pub correct_count: usize,
    pub accuracy: Option<f64>,
    pub mean_error_ms: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OracleFixtureResearch {
    pub sample_id: String,
    pub family: String,
    pub neural_available: bool,
    pub joint_dominance: String,
    pub beat_backend: String,
    pub tempo_backend: String,
    pub grid_phase_backend: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BackendOracleReport {
    pub schema_version: u32,
    pub evaluator: String,
    pub split: String,
    pub source_commit: Option<String>,
    pub material_thresholds: BackendMaterialThresholds,
    pub always_classical: ResearchBackendSummary,
    pub current_hybrid: ResearchBackendSummary,
    pub always_neural_where_available: ResearchBackendSummary,
    pub beat_oracle: BeatOracleSummary,
    pub tempo_oracle: TempoOracleSummary,
    pub grid_phase_oracle: GridPhaseOracleSummary,
    pub joint_outcome_counts: BTreeMap<String, usize>,
    pub neural_dominates: Vec<String>,
    pub classical_dominates: Vec<String>,
    pub per_fixture: Vec<OracleFixtureResearch>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GateFeatureRow {
    pub sample_id: String,
    pub family: String,
    pub pcm_group: String,
    pub lineage_group: String,
    pub leakage_group: String,
    pub neural_available: bool,
    /// This map is deliberately limited to pre-decision neural diagnostics;
    /// it contains no fixture ID, family, transform, or Ground Truth field.
    pub features: BTreeMap<String, f64>,
    pub diagnostic_score: f64,
    pub truth_side_label: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GateValidationFold {
    pub grouping_rule: String,
    pub train_split: Vec<String>,
    pub validation_split: Vec<String>,
    pub train_groups: Vec<String>,
    pub validation_groups: Vec<String>,
    pub threshold: f64,
    pub threshold_reason: String,
    pub validation_neural_coverage: usize,
    pub validation_classical_use_count: usize,
    pub false_accept_bad_neural_count: usize,
    pub false_reject_useful_neural_count: usize,
    pub exact_pcm_overlap: bool,
    pub lineage_overlap: bool,
    pub overlapping_pcm_hashes: Vec<String>,
    pub overlapping_lineage_groups: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GateOofDecision {
    pub sample_id: String,
    pub leakage_group: String,
    pub validation_fold: usize,
    pub fitted_threshold: f64,
    pub diagnostic_score: f64,
    pub decision: String,
    pub truth_side_label: String,
    pub false_accept_bad_neural: bool,
    pub false_reject_useful_neural: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GateResearchReport {
    pub schema_version: u32,
    pub evaluator: String,
    pub split: String,
    pub source_commit: Option<String>,
    pub feature_names: Vec<String>,
    pub validation_grouping: String,
    pub leakage_group_count: usize,
    pub largest_leakage_group_size: usize,
    pub exact_pcm_duplicate_leakage: bool,
    pub lineage_leakage: bool,
    pub current_hybrid_neural_coverage: usize,
    pub current_hybrid_native_regression_count: usize,
    pub always_classical: ResearchBackendSummary,
    pub always_neural_where_available: ResearchBackendSummary,
    pub current_hybrid: ResearchBackendSummary,
    pub beat_oracle: BeatOracleSummary,
    pub tempo_oracle: TempoOracleSummary,
    pub grid_phase_oracle: GridPhaseOracleSummary,
    pub candidate_threshold: Option<f64>,
    pub full_data_refit_threshold: Option<f64>,
    pub full_data_refit_threshold_reason: Option<String>,
    pub full_data_refit_gate: Option<ResearchBackendSummary>,
    pub candidate_gate_neural_coverage: usize,
    pub candidate_gate_native_regression_count: usize,
    pub candidate_gate_fallback_use_classical_count: usize,
    pub candidate_gate: ResearchBackendSummary,
    pub oof_false_accept_bad_neural_count: usize,
    pub oof_false_reject_useful_neural_count: usize,
    pub validation_folds: Vec<GateValidationFold>,
    pub leave_family_out_folds: Vec<GateValidationFold>,
    pub oof_decisions: Vec<GateOofDecision>,
    pub per_fixture: Vec<GateFeatureRow>,
    pub production_recommendation: String,
}

const GATE_FEATURE_NAMES: [&str; 14] = [
    "path_marker_count",
    "path_coverage",
    "activation_mean",
    "support",
    "interval_residual",
    "alias_margin",
    "selected_period_frames",
    "best_candidate_score",
    "best_candidate_margin",
    "selected_candidate_score",
    "selected_candidate_off_grid_leakage",
    "selected_candidate_periodic_consistency",
    "best_vs_selected_relation_disagreement",
    "downbeat_confidence",
];

fn gate_features(track: &TrackEvaluation) -> (BTreeMap<String, f64>, f64, bool) {
    let mut features = BTreeMap::new();
    let Some(diagnostics) = track.neural_diagnostics.as_ref() else {
        return (features, 0.0, false);
    };
    let best_candidate = diagnostics
        .candidates
        .iter()
        .filter(|candidate| candidate.available)
        .max_by(|left, right| left.candidate_score.total_cmp(&right.candidate_score));
    let mut candidate_scores = diagnostics
        .candidates
        .iter()
        .filter(|candidate| candidate.available)
        .map(|candidate| candidate.candidate_score)
        .collect::<Vec<_>>();
    candidate_scores.sort_by(f32::total_cmp);
    let best_candidate_score = best_candidate
        .map(|value| value.candidate_score)
        .unwrap_or_default();
    let best_candidate_margin = candidate_scores
        .iter()
        .rev()
        .nth(1)
        .map_or(0.0, |second| best_candidate_score - second);
    let selected_candidate = diagnostics.selected_period_frames.and_then(|period| {
        diagnostics
            .candidates
            .iter()
            .find(|candidate| candidate.available && candidate.period_frames == period)
    });
    let disagreement = best_candidate
        .map(|candidate| {
            (selected_candidate.map(|selected| selected.relation.as_str())
                != Some(candidate.relation.as_str())) as u8 as f64
        })
        .unwrap_or(0.0);
    let selected_candidate_score = selected_candidate
        .map(|candidate| candidate.candidate_score)
        .unwrap_or_default();
    let selected_candidate_off_grid_leakage = selected_candidate
        .map(|candidate| candidate.off_grid_leakage)
        .unwrap_or(1.0);
    let selected_candidate_periodic_consistency = selected_candidate
        .map(|candidate| candidate.periodic_consistency)
        .unwrap_or_default();
    let values = [
        ("path_marker_count", diagnostics.path_marker_count as f64),
        (
            "path_coverage",
            f64::from(diagnostics.path_coverage.unwrap_or_default()),
        ),
        (
            "activation_mean",
            f64::from(diagnostics.activation_mean.unwrap_or_default()),
        ),
        (
            "support",
            f64::from(diagnostics.support.unwrap_or_default()),
        ),
        (
            "interval_residual",
            f64::from(diagnostics.interval_residual.unwrap_or(1.0)),
        ),
        (
            "alias_margin",
            f64::from(diagnostics.alias_margin.unwrap_or_default()),
        ),
        (
            "selected_period_frames",
            diagnostics.selected_period_frames.unwrap_or_default() as f64,
        ),
        ("best_candidate_score", f64::from(best_candidate_score)),
        ("best_candidate_margin", f64::from(best_candidate_margin)),
        (
            "selected_candidate_score",
            f64::from(selected_candidate_score),
        ),
        (
            "selected_candidate_off_grid_leakage",
            f64::from(selected_candidate_off_grid_leakage),
        ),
        (
            "selected_candidate_periodic_consistency",
            f64::from(selected_candidate_periodic_consistency),
        ),
        ("best_vs_selected_relation_disagreement", disagreement),
        (
            "downbeat_confidence",
            f64::from(track.meter_evidence.downbeat_confidence.unwrap_or_default()),
        ),
    ];
    for (name, value) in values {
        features.insert(name.into(), value);
    }
    let normalized_residual = (1.0
        - features
            .get("interval_residual")
            .copied()
            .unwrap_or(1.0)
            .min(1.0))
    .clamp(0.0, 1.0);
    let score = (0.12 * (features["path_coverage"]).clamp(0.0, 1.0)
        + 0.12 * features["activation_mean"].clamp(0.0, 1.0)
        + 0.12 * features["support"].clamp(0.0, 1.0)
        + 0.10 * features["alias_margin"].clamp(0.0, 1.0)
        + 0.12 * features["best_candidate_score"].clamp(0.0, 1.0)
        + 0.10 * features["best_candidate_margin"].clamp(0.0, 1.0)
        + 0.08 * features["selected_candidate_periodic_consistency"].clamp(0.0, 1.0)
        + 0.07 * normalized_residual
        + 0.05 * features["downbeat_confidence"].clamp(0.0, 1.0)
        - 0.08 * features["selected_candidate_off_grid_leakage"].clamp(0.0, 1.0)
        - 0.04 * features["best_vs_selected_relation_disagreement"])
        .clamp(0.0, 1.0);
    (features, score, track.analysis_backend == "native_neural")
}

fn gate_label(item: &BackendFixtureComparison) -> &'static str {
    if item.hybrid_backend != "native_neural" {
        return "classical_dominates";
    }
    truth_dominance(
        &item.hybrid,
        &item.classical,
        &BackendMaterialThresholds {
            beat_mae_ms: 1.0,
            beat_p95_ms: 5.0,
            precision_recall: 0.02,
        },
    )
}

const SAFE_CLASSICAL_THRESHOLD: f64 = 1.000001;

fn fit_gate_threshold_with_reason(rows: &[GateFeatureRow]) -> (f64, String) {
    if rows.is_empty() {
        return (
            SAFE_CLASSICAL_THRESHOLD,
            "empty training fold; choose Always Classical".into(),
        );
    }
    let neural_rows = rows.iter().filter(|row| row.neural_available).count();
    if neural_rows == 0 {
        return (
            SAFE_CLASSICAL_THRESHOLD,
            "no native-neural training rows; choose Always Classical".into(),
        );
    }
    let useful_rows = rows
        .iter()
        .filter(|row| row.neural_available && row.truth_side_label == "neural_dominates")
        .count();
    if useful_rows == 0 {
        return (
            SAFE_CLASSICAL_THRESHOLD,
            "no neural-dominates training labels; choose Always Classical".into(),
        );
    }
    let mut candidates = vec![0.0, 1.0];
    candidates.extend(rows.iter().map(|row| row.diagnostic_score));
    candidates.sort_by(f64::total_cmp);
    candidates.dedup_by(|left, right| (*left - *right).abs() < f64::EPSILON);
    (
        candidates
            .into_iter()
            .min_by(|left, right| {
                let left_bad = rows
                    .iter()
                    .filter(|row| {
                        row.neural_available
                            && row.diagnostic_score >= *left
                            && row.truth_side_label == "classical_dominates"
                    })
                    .count();
                let right_bad = rows
                    .iter()
                    .filter(|row| {
                        row.neural_available
                            && row.diagnostic_score >= *right
                            && row.truth_side_label == "classical_dominates"
                    })
                    .count();
                let left_useful = rows
                    .iter()
                    .filter(|row| {
                        row.neural_available
                            && row.diagnostic_score >= *left
                            && row.truth_side_label == "neural_dominates"
                    })
                    .count();
                let right_useful = rows
                    .iter()
                    .filter(|row| {
                        row.neural_available
                            && row.diagnostic_score >= *right
                            && row.truth_side_label == "neural_dominates"
                    })
                    .count();
                left_bad
                    .cmp(&right_bad)
                    .then_with(|| right_useful.cmp(&left_useful))
                    .then_with(|| left.total_cmp(right))
            })
            .unwrap_or(SAFE_CLASSICAL_THRESHOLD),
        "fit on native-neural rows with risk-first labels".into(),
    )
}

fn gate_validation_fold(
    rows: &[GateFeatureRow],
    validation_indices: &[usize],
    grouping_rule: &str,
) -> GateValidationFold {
    let validation = validation_indices.iter().copied().collect::<BTreeSet<_>>();
    let train_indices = (0..rows.len())
        .filter(|index| !validation.contains(index))
        .collect::<Vec<_>>();
    let train_rows = train_indices
        .iter()
        .map(|index| rows[*index].clone())
        .collect::<Vec<_>>();
    let (threshold, threshold_reason) = fit_gate_threshold_with_reason(&train_rows);
    let false_accept = validation_indices
        .iter()
        .filter(|index| {
            let row = &rows[**index];
            row.neural_available
                && row.diagnostic_score >= threshold
                && row.truth_side_label == "classical_dominates"
        })
        .count();
    let false_reject = validation_indices
        .iter()
        .filter(|index| {
            let row = &rows[**index];
            row.neural_available
                && row.diagnostic_score < threshold
                && row.truth_side_label == "neural_dominates"
        })
        .count();
    let validation_neural_coverage = validation_indices
        .iter()
        .filter(|index| {
            let row = &rows[**index];
            row.neural_available && row.diagnostic_score >= threshold
        })
        .count();
    let train_groups = train_indices
        .iter()
        .map(|index| gate_group(&rows[*index], grouping_rule))
        .collect::<BTreeSet<_>>();
    let validation_groups = validation_indices
        .iter()
        .map(|index| gate_group(&rows[*index], grouping_rule))
        .collect::<BTreeSet<_>>();
    let train_pcm_hashes = train_indices
        .iter()
        .map(|index| rows[*index].pcm_group.clone())
        .collect::<BTreeSet<_>>();
    let validation_pcm_hashes = validation_indices
        .iter()
        .map(|index| rows[*index].pcm_group.clone())
        .collect::<BTreeSet<_>>();
    let overlapping_pcm_hashes = train_pcm_hashes
        .intersection(&validation_pcm_hashes)
        .cloned()
        .collect::<Vec<_>>();
    let train_lineage_groups = train_indices
        .iter()
        .map(|index| rows[*index].lineage_group.clone())
        .collect::<BTreeSet<_>>();
    let validation_lineage_groups = validation_indices
        .iter()
        .map(|index| rows[*index].lineage_group.clone())
        .collect::<BTreeSet<_>>();
    let overlapping_lineage_groups = train_lineage_groups
        .intersection(&validation_lineage_groups)
        .cloned()
        .collect::<Vec<_>>();
    GateValidationFold {
        grouping_rule: grouping_rule.into(),
        train_split: train_indices
            .iter()
            .map(|index| rows[*index].sample_id.clone())
            .collect(),
        validation_split: validation_indices
            .iter()
            .map(|index| rows[*index].sample_id.clone())
            .collect(),
        train_groups: train_groups.iter().cloned().collect(),
        validation_groups: validation_groups.iter().cloned().collect(),
        threshold,
        threshold_reason,
        validation_neural_coverage,
        validation_classical_use_count: validation_indices.len() - validation_neural_coverage,
        false_accept_bad_neural_count: false_accept,
        false_reject_useful_neural_count: false_reject,
        exact_pcm_overlap: !overlapping_pcm_hashes.is_empty(),
        lineage_overlap: !overlapping_lineage_groups.is_empty(),
        overlapping_pcm_hashes,
        overlapping_lineage_groups,
    }
}

fn gate_group(row: &GateFeatureRow, grouping_rule: &str) -> String {
    match grouping_rule {
        "joint_leakage" => row.leakage_group.clone(),
        "component_expanded_leave_family_out" => row.leakage_group.clone(),
        "exact_pcm" => row.pcm_group.clone(),
        "lineage" => row.lineage_group.clone(),
        "family" => row.family.clone(),
        _ => row.sample_id.clone(),
    }
}

/// Hold out every connected PCM+lineage component touched by a nominal
/// family. Family labels are not safe leakage boundaries on their own.
fn component_expanded_leave_family_out_folds(rows: &[GateFeatureRow]) -> Vec<GateValidationFold> {
    let families = rows
        .iter()
        .map(|row| row.family.clone())
        .collect::<BTreeSet<_>>();
    let mut components_by_family = BTreeMap::<String, BTreeSet<String>>::new();
    for row in rows {
        components_by_family
            .entry(row.family.clone())
            .or_default()
            .insert(row.leakage_group.clone());
    }
    families
        .into_iter()
        .filter_map(|requested_family| {
            let held_out_components = components_by_family
                .get(&requested_family)
                .cloned()
                .unwrap_or_default();
            let validation_indices = rows
                .iter()
                .enumerate()
                .filter(|(_, row)| held_out_components.contains(&row.leakage_group))
                .map(|(index, _)| index)
                .collect::<Vec<_>>();
            if validation_indices.is_empty() || validation_indices.len() == rows.len() {
                return None;
            }
            let mut fold = gate_validation_fold(
                rows,
                &validation_indices,
                "component_expanded_leave_family_out",
            );
            fold.validation_groups = vec![requested_family];
            Some(fold)
        })
        .collect()
}

#[derive(Clone, Debug)]
struct UnionFind {
    parent: Vec<usize>,
    rank: Vec<usize>,
}

impl UnionFind {
    fn new(size: usize) -> Self {
        Self {
            parent: (0..size).collect(),
            rank: vec![0; size],
        }
    }

    fn find(&mut self, index: usize) -> usize {
        if self.parent[index] != index {
            let root = self.find(self.parent[index]);
            self.parent[index] = root;
        }
        self.parent[index]
    }

    fn union(&mut self, left: usize, right: usize) {
        let mut left = self.find(left);
        let mut right = self.find(right);
        if left == right {
            return;
        }
        if self.rank[left] < self.rank[right] {
            std::mem::swap(&mut left, &mut right);
        }
        self.parent[right] = left;
        if self.rank[left] == self.rank[right] {
            self.rank[left] += 1;
        }
    }
}

fn connected_leakage_groups(rows: &[GateFeatureRow]) -> BTreeMap<String, String> {
    let mut union_find = UnionFind::new(rows.len());
    let mut first_pcm = BTreeMap::<String, usize>::new();
    let mut first_lineage = BTreeMap::<String, usize>::new();
    for (index, row) in rows.iter().enumerate() {
        if let Some(previous) = first_pcm.insert(row.pcm_group.clone(), index) {
            union_find.union(previous, index);
        }
        if let Some(previous) = first_lineage.insert(row.lineage_group.clone(), index) {
            union_find.union(previous, index);
        }
    }
    let mut canonical_by_root = BTreeMap::<usize, String>::new();
    for (index, _) in rows.iter().enumerate() {
        let root = union_find.find(index);
        canonical_by_root
            .entry(root)
            .and_modify(|sample_id| {
                if rows[index].sample_id < *sample_id {
                    *sample_id = rows[index].sample_id.clone();
                }
            })
            .or_insert_with(|| rows[index].sample_id.clone());
    }
    rows.iter()
        .enumerate()
        .map(|(index, row)| {
            let root = union_find.find(index);
            (
                row.sample_id.clone(),
                canonical_by_root.get(&root).cloned().unwrap_or_default(),
            )
        })
        .collect()
}

fn grouped_validation_folds(
    rows: &[GateFeatureRow],
    grouping_rule: &str,
) -> Vec<GateValidationFold> {
    let mut groups = BTreeMap::<String, Vec<usize>>::new();
    for (index, row) in rows.iter().enumerate() {
        groups
            .entry(gate_group(row, grouping_rule))
            .or_default()
            .push(index);
    }
    groups
        .into_values()
        .map(|indices| gate_validation_fold(rows, &indices, grouping_rule))
        .collect()
}

fn build_gate_research(
    report: &EvaluationReport,
    pcm_groups: &BTreeMap<String, String>,
    lineage_groups: &BTreeMap<String, String>,
    source_commit: Option<String>,
) -> GateResearchReport {
    let comparisons = report
        .backend_comparison
        .per_fixture
        .iter()
        .map(|item| (item.sample_id.as_str(), item))
        .collect::<BTreeMap<_, _>>();
    let mut rows = Vec::new();
    for track in &report.per_track {
        let Some(comparison) = comparisons.get(track.sample_id.as_str()) else {
            continue;
        };
        let (features, diagnostic_score, neural_available) = gate_features(track);
        rows.push(GateFeatureRow {
            sample_id: track.sample_id.clone(),
            family: track.family.clone(),
            pcm_group: pcm_groups
                .get(&track.sample_id)
                .cloned()
                .unwrap_or_else(|| track.sample_id.clone()),
            lineage_group: lineage_groups
                .get(&track.sample_id)
                .cloned()
                .unwrap_or_else(|| track.sample_id.clone()),
            leakage_group: String::new(),
            neural_available,
            features,
            diagnostic_score,
            truth_side_label: gate_label(comparison).into(),
        });
    }
    rows.sort_by(|left, right| left.sample_id.cmp(&right.sample_id));
    let leakage_groups = connected_leakage_groups(&rows);
    for row in &mut rows {
        row.leakage_group = leakage_groups
            .get(&row.sample_id)
            .cloned()
            .unwrap_or_else(|| row.sample_id.clone());
    }
    let primary_folds = grouped_validation_folds(&rows, "joint_leakage");
    let family_validation = component_expanded_leave_family_out_folds(&rows);
    let exact_pcm_duplicate_leakage = primary_folds.iter().any(|fold| fold.exact_pcm_overlap);
    let lineage_leakage = primary_folds.iter().any(|fold| fold.lineage_overlap);
    let largest_leakage_group_size = rows
        .iter()
        .fold(BTreeMap::<String, usize>::new(), |mut counts, row| {
            *counts.entry(row.leakage_group.clone()).or_default() += 1;
            counts
        })
        .values()
        .copied()
        .max()
        .unwrap_or_default();

    let mut oof_decisions = Vec::new();
    let mut selected_oof_metrics = Vec::new();
    let mut candidate_coverage = 0;
    let mut candidate_regressions = 0;
    let mut fallback = 0;
    let mut false_accept = 0;
    let mut false_reject = 0;
    for (fold_index, fold) in primary_folds.iter().enumerate() {
        let validation_indices = rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| {
                fold.validation_split
                    .contains(&row.sample_id)
                    .then_some(index)
            })
            .collect::<Vec<_>>();
        let train_indices = rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| {
                (!fold.validation_split.contains(&row.sample_id)).then_some(index)
            })
            .collect::<Vec<_>>();
        let train_rows = train_indices
            .iter()
            .map(|index| rows[*index].clone())
            .collect::<Vec<_>>();
        let (threshold, _) = fit_gate_threshold_with_reason(&train_rows);
        for index in validation_indices {
            let row = &rows[index];
            let comparison = comparisons[row.sample_id.as_str()];
            let accept = row.neural_available && row.diagnostic_score >= threshold;
            let false_accept_row = accept && row.truth_side_label == "classical_dominates";
            let false_reject_row = !accept && row.truth_side_label == "neural_dominates";
            let decision = if accept { "neural" } else { "classical" };
            if accept {
                candidate_coverage += 1;
                selected_oof_metrics.push(&comparison.hybrid_metrics);
                if false_accept_row {
                    candidate_regressions += 1;
                }
            } else {
                fallback += 1;
                selected_oof_metrics.push(&comparison.classical_metrics);
            }
            false_accept += usize::from(false_accept_row);
            false_reject += usize::from(false_reject_row);
            oof_decisions.push(GateOofDecision {
                sample_id: row.sample_id.clone(),
                leakage_group: row.leakage_group.clone(),
                validation_fold: fold_index,
                fitted_threshold: threshold,
                diagnostic_score: row.diagnostic_score,
                decision: decision.into(),
                truth_side_label: row.truth_side_label.clone(),
                false_accept_bad_neural: false_accept_row,
                false_reject_useful_neural: false_reject_row,
            });
        }
    }
    oof_decisions.sort_by(|left, right| left.sample_id.cmp(&right.sample_id));
    let full_data_refit_threshold = fit_gate_threshold_with_reason(&rows);
    let mut full_data_selected = Vec::new();
    for row in &rows {
        let comparison = comparisons[row.sample_id.as_str()];
        if row.neural_available && row.diagnostic_score >= full_data_refit_threshold.0 {
            full_data_selected.push(&comparison.hybrid_metrics);
        } else {
            full_data_selected.push(&comparison.classical_metrics);
        }
    }
    let neural_coverage = report
        .backend_comparison
        .per_fixture
        .iter()
        .filter(|item| item.hybrid_backend == "native_neural")
        .count();
    let current_regressions = report
        .backend_comparison
        .per_fixture
        .iter()
        .filter(|item| item.outcome == "native_neural_regressed")
        .count();
    let all_classical = report
        .backend_comparison
        .per_fixture
        .iter()
        .map(|item| &item.classical_metrics)
        .collect::<Vec<_>>();
    let all_neural = report
        .backend_comparison
        .per_fixture
        .iter()
        .filter(|item| item.hybrid_backend == "native_neural")
        .map(|item| &item.hybrid_metrics)
        .collect::<Vec<_>>();
    let current = report
        .backend_comparison
        .per_fixture
        .iter()
        .map(|item| &item.hybrid_metrics)
        .collect::<Vec<_>>();
    let oracle_report = build_backend_oracle(report, source_commit.clone());
    let beat_oracle = oracle_report.beat_oracle;
    let tempo_oracle = oracle_report.tempo_oracle;
    let grid_phase_oracle = oracle_report.grid_phase_oracle;
    GateResearchReport {
        schema_version: RESEARCH_REPORT_SCHEMA_VERSION,
        evaluator: format!("wotoha-analysis-lab/{}", env!("CARGO_PKG_VERSION")),
        split: report.split.clone(),
        source_commit,
        feature_names: GATE_FEATURE_NAMES.iter().map(|name| (*name).into()).collect(),
        validation_grouping: "primary leave-one-connected-PCM+lineage-group-out; leave-family-out secondary".into(),
        leakage_group_count: rows
            .iter()
            .map(|row| row.leakage_group.as_str())
            .collect::<BTreeSet<_>>()
            .len(),
        largest_leakage_group_size,
        exact_pcm_duplicate_leakage,
        lineage_leakage,
        current_hybrid_neural_coverage: neural_coverage,
        current_hybrid_native_regression_count: current_regressions,
        always_classical: summary_from_metric_refs(&all_classical),
        always_neural_where_available: summary_from_metric_refs(&all_neural),
        current_hybrid: summary_from_metric_refs(&current),
        beat_oracle,
        tempo_oracle,
        grid_phase_oracle,
        candidate_threshold: None,
        full_data_refit_threshold: Some(full_data_refit_threshold.0),
        full_data_refit_threshold_reason: Some(full_data_refit_threshold.1),
        full_data_refit_gate: Some(summary_from_metric_refs(&full_data_selected)),
        candidate_gate_neural_coverage: candidate_coverage,
        candidate_gate_native_regression_count: candidate_regressions,
        candidate_gate_fallback_use_classical_count: fallback,
        candidate_gate: summary_from_metric_refs(&selected_oof_metrics),
        oof_false_accept_bad_neural_count: false_accept,
        oof_false_reject_useful_neural_count: false_reject,
        validation_folds: primary_folds,
        leave_family_out_folds: family_validation,
        oof_decisions,
        per_fixture: rows,
        production_recommendation: "do-not-promote: research-only gate; compare against Always Classical on held-out groups".into(),
    }
}

fn summary_from_metric_refs(metrics: &[&GroupMetrics]) -> ResearchBackendSummary {
    let aggregate = aggregate_groups(metrics.iter().copied());
    summary_from_group_metrics(&aggregate)
}

fn summary_from_group_metrics(metrics: &GroupMetrics) -> ResearchBackendSummary {
    ResearchBackendSummary {
        available_tracks: metrics.tracks,
        beat_mae_ms: metrics.beat.mae_ms,
        beat_p50_ms: metrics.beat.p50_ms,
        beat_p95_ms: metrics.beat.p95_ms,
        precision_at_tolerance: metrics.beat.precision_at_tolerance.clone(),
        recall_at_tolerance: metrics.beat.recall_at_tolerance.clone(),
        precision_at_40ms: metrics.beat.precision_at_tolerance.get("40ms").copied(),
        recall_at_40ms: metrics.beat.recall_at_tolerance.get("40ms").copied(),
        tempo_scored: metrics.tempo.scored_tracks,
        tempo_correct: metrics.tempo.primary_correct_count,
        tempo_accuracy: metrics.tempo.primary_correct_rate,
        grid_phase_scored: metrics.grid_phase.scored_tracks,
        grid_phase_correct: metrics.grid_phase.correct_count,
        grid_phase_accuracy: metrics.grid_phase.correct_rate,
    }
}

fn truth_dominance(
    neural: &BackendMetricSnapshot,
    classical: &BackendMetricSnapshot,
    thresholds: &BackendMaterialThresholds,
) -> &'static str {
    if !neural.core_valid && !classical.core_valid {
        return "both_invalid";
    }
    if neural.core_valid && !classical.core_valid {
        return "neural_dominates";
    }
    if classical.core_valid && !neural.core_valid {
        return "classical_dominates";
    }
    let mut neural_better = false;
    let mut classical_better = false;
    for (neural_value, classical_value, threshold, lower_is_better) in [
        (
            neural.beat_mae_ms,
            classical.beat_mae_ms,
            thresholds.beat_mae_ms,
            true,
        ),
        (
            neural.beat_p95_ms,
            classical.beat_p95_ms,
            thresholds.beat_p95_ms,
            true,
        ),
    ] {
        if let (Some(neural_value), Some(classical_value)) = (neural_value, classical_value)
            && lower_is_better
        {
            neural_better |= neural_value <= classical_value - threshold;
            classical_better |= classical_value <= neural_value - threshold;
        }
    }
    for (neural_value, classical_value) in [
        (neural.precision_at_40ms, classical.precision_at_40ms),
        (neural.recall_at_40ms, classical.recall_at_40ms),
    ] {
        if let (Some(neural_value), Some(classical_value)) = (neural_value, classical_value) {
            neural_better |= neural_value >= classical_value + thresholds.precision_recall;
            classical_better |= classical_value >= neural_value + thresholds.precision_recall;
        }
    }
    if neural.primary_tempo_correct == Some(true) && classical.primary_tempo_correct == Some(false)
    {
        neural_better = true;
    }
    if classical.primary_tempo_correct == Some(true) && neural.primary_tempo_correct == Some(false)
    {
        classical_better = true;
    }
    if neural.grid_phase_correct == Some(true) && classical.grid_phase_correct == Some(false) {
        neural_better = true;
    }
    if classical.grid_phase_correct == Some(true) && neural.grid_phase_correct == Some(false) {
        classical_better = true;
    }
    match (neural_better, classical_better) {
        (true, false) => "neural_dominates",
        (false, true) => "classical_dominates",
        (false, false) => "equal",
        (true, true) => "mixed",
    }
}

fn choose_backend_for_lower(left: Option<f64>, right: Option<f64>) -> &'static str {
    match (left, right) {
        (Some(left), Some(right)) if left < right => "neural",
        (Some(left), Some(right)) if right < left => "classical",
        (Some(_), None) => "neural",
        (None, Some(_)) => "classical",
        _ => "equal",
    }
}

fn choose_backend_for_tempo(
    neural_available: bool,
    neural: &BackendMetricSnapshot,
    classical: &BackendMetricSnapshot,
) -> &'static str {
    if !neural_available {
        return "classical";
    }
    match (
        neural.primary_tempo_correct,
        classical.primary_tempo_correct,
    ) {
        (Some(true), Some(false)) => "neural",
        (Some(false), Some(true)) => "classical",
        (Some(true), Some(true)) => "equal",
        (Some(_), None) => "neural",
        (None, Some(_)) => "classical",
        (Some(false), Some(false)) => match (
            neural.tempo_absolute_error_bpm,
            classical.tempo_absolute_error_bpm,
        ) {
            (Some(left), Some(right)) if left < right => "neural",
            (Some(left), Some(right)) if right < left => "classical",
            _ => "equal",
        },
        _ => "invalid",
    }
}

fn choose_backend_for_grid_phase(
    neural_available: bool,
    neural: &BackendMetricSnapshot,
    classical: &BackendMetricSnapshot,
) -> &'static str {
    if !neural_available {
        return "classical";
    }
    match (neural.grid_phase_correct, classical.grid_phase_correct) {
        (Some(true), Some(false)) => "neural",
        (Some(false), Some(true)) => "classical",
        (Some(true), Some(true)) => "equal",
        (Some(_), None) => "neural",
        (None, Some(_)) => "classical",
        (Some(false), Some(false)) => {
            match (neural.grid_phase_error_ms, classical.grid_phase_error_ms) {
                (Some(left), Some(right)) if left < right => "neural",
                (Some(left), Some(right)) if right < left => "classical",
                _ => "equal",
            }
        }
        _ => "invalid",
    }
}

fn beat_oracle_summary(metrics: &[&GroupMetrics]) -> BeatOracleSummary {
    let aggregate = aggregate_groups(metrics.iter().copied());
    let precision_at_40ms = aggregate.beat.precision_at_tolerance.get("40ms").copied();
    let recall_at_40ms = aggregate.beat.recall_at_tolerance.get("40ms").copied();
    BeatOracleSummary {
        available_tracks: aggregate.tracks,
        beat_mae_ms: aggregate.beat.mae_ms,
        beat_p50_ms: aggregate.beat.p50_ms,
        beat_p95_ms: aggregate.beat.p95_ms,
        precision_at_tolerance: aggregate.beat.precision_at_tolerance,
        recall_at_tolerance: aggregate.beat.recall_at_tolerance,
        precision_at_40ms,
        recall_at_40ms,
    }
}

fn tempo_oracle_summary(metrics: &[&GroupMetrics]) -> TempoOracleSummary {
    let aggregate = aggregate_groups(metrics.iter().copied());
    TempoOracleSummary {
        scored_tracks: aggregate.tempo.scored_tracks,
        correct_count: aggregate.tempo.primary_correct_count,
        accuracy: aggregate.tempo.primary_correct_rate,
    }
}

fn grid_phase_oracle_summary(metrics: &[&GroupMetrics]) -> GridPhaseOracleSummary {
    let aggregate = aggregate_groups(metrics.iter().copied());
    GridPhaseOracleSummary {
        scored_tracks: aggregate.grid_phase.scored_tracks,
        correct_count: aggregate.grid_phase.correct_count,
        accuracy: aggregate.grid_phase.correct_rate,
        mean_error_ms: aggregate.grid_phase.error_ms,
    }
}

fn build_backend_oracle(
    report: &EvaluationReport,
    source_commit: Option<String>,
) -> BackendOracleReport {
    let thresholds = BackendMaterialThresholds {
        beat_mae_ms: 1.0,
        beat_p95_ms: 5.0,
        precision_recall: 0.02,
    };
    let all_classical = report
        .backend_comparison
        .per_fixture
        .iter()
        .map(|item| &item.classical_metrics)
        .collect::<Vec<_>>();
    let current_hybrid = report
        .backend_comparison
        .per_fixture
        .iter()
        .map(|item| &item.hybrid_metrics)
        .collect::<Vec<_>>();
    let always_neural = report
        .backend_comparison
        .per_fixture
        .iter()
        .filter(|item| item.hybrid_backend == "native_neural")
        .map(|item| &item.hybrid_metrics)
        .collect::<Vec<_>>();
    let mut oracle = Vec::new();
    let mut beat_oracle_metrics = Vec::new();
    let mut tempo_oracle_metrics = Vec::new();
    let mut grid_phase_oracle_metrics = Vec::new();
    let mut joint_outcome_counts = BTreeMap::new();
    let mut neural_dominates = Vec::new();
    let mut classical_dominates = Vec::new();
    for item in &report.backend_comparison.per_fixture {
        let neural_available = item.hybrid_backend == "native_neural";
        let joint = if neural_available {
            truth_dominance(&item.hybrid, &item.classical, &thresholds)
        } else if item.classical.core_valid {
            "classical_dominates"
        } else {
            "both_invalid"
        };
        *joint_outcome_counts.entry(joint.into()).or_default() += 1;
        if joint == "neural_dominates" {
            neural_dominates.push(item.sample_id.clone());
        }
        if joint == "classical_dominates" {
            classical_dominates.push(item.sample_id.clone());
        }
        let beat_backend = if neural_available
            && choose_backend_for_lower(item.hybrid.beat_mae_ms, item.classical.beat_mae_ms)
                == "neural"
        {
            beat_oracle_metrics.push(&item.hybrid_metrics);
            "neural"
        } else {
            beat_oracle_metrics.push(&item.classical_metrics);
            "classical"
        };
        let tempo_backend_choice =
            choose_backend_for_tempo(neural_available, &item.hybrid, &item.classical);
        if tempo_backend_choice == "neural" {
            tempo_oracle_metrics.push(&item.hybrid_metrics);
        } else {
            tempo_oracle_metrics.push(&item.classical_metrics);
        }
        let grid_phase_backend_choice =
            choose_backend_for_grid_phase(neural_available, &item.hybrid, &item.classical);
        if grid_phase_backend_choice == "neural" {
            grid_phase_oracle_metrics.push(&item.hybrid_metrics);
        } else {
            grid_phase_oracle_metrics.push(&item.classical_metrics);
        }
        oracle.push(OracleFixtureResearch {
            sample_id: item.sample_id.clone(),
            family: item.family.clone(),
            neural_available,
            joint_dominance: joint.into(),
            beat_backend: beat_backend.into(),
            tempo_backend: tempo_backend_choice.into(),
            grid_phase_backend: grid_phase_backend_choice.into(),
        });
    }
    BackendOracleReport {
        schema_version: RESEARCH_REPORT_SCHEMA_VERSION,
        evaluator: format!("wotoha-analysis-lab/{}", env!("CARGO_PKG_VERSION")),
        split: report.split.clone(),
        source_commit,
        material_thresholds: thresholds,
        always_classical: summary_from_metric_refs(&all_classical),
        current_hybrid: summary_from_metric_refs(&current_hybrid),
        always_neural_where_available: summary_from_metric_refs(&always_neural),
        beat_oracle: beat_oracle_summary(&beat_oracle_metrics),
        tempo_oracle: tempo_oracle_summary(&tempo_oracle_metrics),
        grid_phase_oracle: grid_phase_oracle_summary(&grid_phase_oracle_metrics),
        joint_outcome_counts,
        neural_dominates,
        classical_dominates,
        per_fixture: oracle,
    }
}

fn experimental_tempo_resolution(
    fixture: &SyntheticFixture,
    prediction: &NormalizedAnalysis,
) -> ExperimentalTempoResolution {
    let mut seeds = Vec::new();
    let base_bpm = prediction
        .tempo_hypotheses
        .iter()
        .find(|hypothesis| canonical_relation_name(&hypothesis.relation) == "primary")
        .or_else(|| prediction.tempo_hypotheses.first())
        .map(|hypothesis| hypothesis.bpm);
    if let Some(base_bpm) = base_bpm.filter(|bpm| bpm.is_finite() && *bpm > 0.0) {
        seeds.extend([
            (base_bpm / 2.0, "half_time".to_owned()),
            (base_bpm, "primary".to_owned()),
            (base_bpm * 2.0, "double_time".to_owned()),
        ]);
    }
    seeds.extend(prediction.tempo_hypotheses.iter().map(|hypothesis| {
        (
            hypothesis.bpm,
            canonical_relation_name(&hypothesis.relation).to_owned(),
        )
    }));

    let mut candidates = Vec::new();
    for (bpm, relation) in seeds {
        if !bpm.is_finite()
            || bpm <= 0.0
            || candidates
                .iter()
                .any(|candidate: &ExperimentalTempoCandidate| (candidate.bpm - bpm).abs() < 0.01)
        {
            continue;
        }
        let available = (wotoha_core::beat_analysis::MIN_BPM..=wotoha_core::beat_analysis::MAX_BPM)
            .contains(&bpm);
        let candidate = if available {
            score_raw_periodic_candidate(fixture, bpm, relation.clone())
                .unwrap_or_else(|| unavailable_tempo_candidate(bpm, relation.clone()))
        } else {
            unavailable_tempo_candidate(bpm, relation)
        };
        candidates.push(candidate);
    }
    candidates.sort_by(|left, right| {
        right
            .available
            .cmp(&left.available)
            .then_with(|| right.score.total_cmp(&left.score))
    });
    let (selected, ambiguous) = choose_experimental_candidate(&candidates);
    ExperimentalTempoResolution {
        selected_bpm: selected.map(|candidate| candidate.bpm),
        selected_relation: selected.map(|candidate| candidate.relation.clone()),
        ambiguous,
        candidates,
    }
}

fn choose_experimental_candidate(
    candidates: &[ExperimentalTempoCandidate],
) -> (Option<&ExperimentalTempoCandidate>, bool) {
    let mut available = candidates
        .iter()
        .filter(|candidate| candidate.available)
        .collect::<Vec<_>>();
    available.sort_by(|left, right| right.score.total_cmp(&left.score));
    let ambiguous = available
        .first()
        .zip(available.get(1))
        .is_some_and(|(best, runner_up)| best.score - runner_up.score < 0.03);
    let selected = (!ambiguous).then(|| available.first()).flatten().copied();
    (selected, ambiguous)
}

fn activation_tempo_resolution(
    observations: &wotoha_core::beat_analysis::NeuralBeatObservations,
    primary_bpm: Option<f32>,
) -> Option<ActivationTempoResolution> {
    let primary_bpm = primary_bpm.filter(|bpm| bpm.is_finite() && *bpm > 0.0)?;
    let primary_period = (observations.frame_rate_hz * 60.0 / primary_bpm).round() as usize;
    if primary_period == 0 {
        return None;
    }
    let mut candidates =
        wotoha_core::beat_analysis::score_neural_tempo_family(observations, primary_period)
            .into_iter()
            .map(|candidate| ActivationTempoCandidate {
                bpm: candidate.bpm,
                relation: neural_relation_name(candidate.relation),
                period_frames: candidate.period_frames,
                available: candidate.available,
                best_phase_frames: candidate.best_phase_frames,
                activation_evidence: candidate.activation_evidence,
                coverage: candidate.coverage,
                off_grid_leakage: candidate.off_grid_leakage,
                periodic_consistency: candidate.periodic_consistency,
                candidate_score: candidate.candidate_score,
            })
            .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        right
            .available
            .cmp(&left.available)
            .then_with(|| right.candidate_score.total_cmp(&left.candidate_score))
            .then_with(|| left.period_frames.cmp(&right.period_frames))
    });
    let mut available = candidates
        .iter()
        .filter(|candidate| candidate.available)
        .collect::<Vec<_>>();
    available.sort_by(|left, right| {
        right
            .candidate_score
            .total_cmp(&left.candidate_score)
            .then_with(|| left.period_frames.cmp(&right.period_frames))
    });
    let ambiguous = available
        .first()
        .zip(available.get(1))
        .is_some_and(|(best, runner_up)| best.candidate_score - runner_up.candidate_score < 0.03);
    let selected = (!ambiguous).then(|| available.first()).flatten();
    Some(ActivationTempoResolution {
        selected_bpm: selected.map(|candidate| candidate.bpm),
        selected_relation: selected.map(|candidate| candidate.relation.clone()),
        ambiguous,
        candidates,
    })
}

fn sigmoid(value: f32) -> f32 {
    if value >= 0.0 {
        1.0 / (1.0 + (-value).exp())
    } else {
        let exponential = value.exp();
        exponential / (1.0 + exponential)
    }
}

fn interpolate(values: &[f32], position: f32) -> f32 {
    if values.is_empty() || !position.is_finite() || position < 0.0 {
        return 0.0;
    }
    let left = position.floor() as usize;
    if left >= values.len() {
        return 0.0;
    }
    let right = (left + 1).min(values.len().saturating_sub(1));
    let fraction = position - left as f32;
    values[left] * (1.0 - fraction) + values[right] * fraction
}

#[derive(Clone, Copy, Debug)]
struct FractionalTempoScore {
    score: f32,
    activation_support: f32,
    coverage: f32,
    off_grid_leakage: f32,
    periodic_consistency: f32,
    phase_stability: f32,
}

fn score_fractional_period(
    activations: &[f32],
    period: f32,
    phase: f32,
) -> Option<FractionalTempoScore> {
    if activations.len() < 8
        || !period.is_finite()
        || !(1.0..=activations.len() as f32).contains(&period)
    {
        return None;
    }
    let probabilities = activations
        .iter()
        .map(|value| sigmoid(*value))
        .collect::<Vec<_>>();
    let mut on_grid = Vec::new();
    let mut position = phase;
    while position < probabilities.len() as f32 {
        on_grid.push(interpolate(&probabilities, position));
        position += period;
    }
    if on_grid.len() < 3 {
        return None;
    }
    let activation_support = on_grid.iter().sum::<f32>() / on_grid.len() as f32;
    let coverage =
        on_grid.iter().filter(|value| **value >= 0.55).count() as f32 / on_grid.len() as f32;
    let mut leakage_sum = 0.0;
    for (index, activation) in probabilities.iter().enumerate() {
        let frame = index as f32;
        let nearest = ((frame - phase) / period).round();
        let distance = (frame - (phase + nearest * period)).abs();
        if distance > 0.35 {
            leakage_sum += *activation;
        }
    }
    let off_grid_leakage = (leakage_sum / probabilities.len() as f32).clamp(0.0, 1.0);
    let mut paired = 0.0;
    let mut energy = 0.0;
    for index in 0..probabilities.len() {
        let next = interpolate(&probabilities, index as f32 + period);
        paired += probabilities[index] * next;
        energy += probabilities[index] * probabilities[index];
    }
    let periodic_consistency = if energy > f32::EPSILON {
        (paired / energy).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let left =
        score_fractional_period_without_stability(activations, period, phase - 0.25).unwrap_or(0.0);
    let right =
        score_fractional_period_without_stability(activations, period, phase + 0.25).unwrap_or(0.0);
    let phase_stability = (1.0 - (left - right).abs()).clamp(0.0, 1.0);
    let score = (0.42 * activation_support
        + 0.18 * coverage
        + 0.18 * periodic_consistency
        + 0.12 * phase_stability
        - 0.35 * off_grid_leakage)
        .clamp(0.0, 1.0);
    Some(FractionalTempoScore {
        score,
        activation_support,
        coverage,
        off_grid_leakage,
        periodic_consistency,
        phase_stability,
    })
}

fn score_fractional_period_without_stability(
    activations: &[f32],
    period: f32,
    phase: f32,
) -> Option<f32> {
    if phase < 0.0 {
        return None;
    }
    let probabilities = activations
        .iter()
        .map(|value| sigmoid(*value))
        .collect::<Vec<_>>();
    let mut position = phase;
    let mut sum = 0.0;
    let mut count = 0;
    while position < probabilities.len() as f32 {
        sum += interpolate(&probabilities, position);
        count += 1;
        position += period;
    }
    (count >= 3).then_some(sum / count as f32)
}

fn fractional_tempo_refinement(
    observations: &wotoha_core::beat_analysis::NeuralBeatObservations,
    current_bpm: Option<f32>,
    relation: &str,
) -> Option<FractionalTempoRefinement> {
    let current_bpm = current_bpm.filter(|value| value.is_finite() && *value > 0.0)?;
    let integer_period = (observations.frame_rate_hz * 60.0 / current_bpm).round() as i32;
    if integer_period < 2 {
        return None;
    }
    let resolution = 0.1_f32;
    let mut best: Option<(f32, f32, FractionalTempoScore)> = None;
    let mut period = (integer_period - 1).max(2) as f32;
    let maximum_period = (integer_period + 1) as f32;
    while period <= maximum_period + f32::EPSILON {
        let mut coarse_best: Option<(f32, FractionalTempoScore)> = None;
        let mut coarse_phase = 0.0;
        while coarse_phase < period {
            if let Some(score) =
                score_fractional_period(&observations.beat_logits, period, coarse_phase)
                && coarse_best
                    .as_ref()
                    .is_none_or(|(_, current)| score.score > current.score)
            {
                coarse_best = Some((coarse_phase, score));
                coarse_phase += 0.5;
                continue;
            }
            coarse_phase += 0.5;
        }
        if let Some((coarse_phase, _)) = coarse_best {
            let start = (coarse_phase - 1.0).max(0.0);
            let end = (coarse_phase + 1.0).min(period);
            let mut phase = start;
            while phase <= end + f32::EPSILON {
                if let Some(score) =
                    score_fractional_period(&observations.beat_logits, period, phase)
                    && best.as_ref().is_none_or(
                        |(_, _, current): &(f32, f32, FractionalTempoScore)| {
                            score.score > current.score
                        },
                    )
                {
                    best = Some((period, phase, score));
                }
                phase += resolution;
            }
        }
        period += resolution;
    }
    let (period, phase, score) = best?;
    Some(FractionalTempoRefinement {
        selected_bpm: Some(observations.frame_rate_hz * 60.0 / period),
        selected_period_frames: Some(period),
        selected_phase_frames: Some(phase),
        relation: relation.to_owned(),
        score: score.score,
        activation_support: score.activation_support,
        coverage: score.coverage,
        off_grid_leakage: score.off_grid_leakage,
        periodic_consistency: score.periodic_consistency,
        phase_stability: score.phase_stability,
        resolution_frames: resolution,
    })
}

fn median_f64(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    Some(sorted[sorted.len() / 2])
}

/// Estimate a tempo from the decoded Neural BeatEvent clock using a bounded,
/// robust interval fit. The production family relation is carried through as
/// metadata; this function never changes half/native/double selection.
fn beat_event_interval_refinement(
    observations: &wotoha_core::beat_analysis::NeuralBeatObservations,
    relation: &str,
) -> BeatEventIntervalRefinement {
    let unavailable = |reason: &str,
                       usable_events: usize,
                       usable_intervals: usize,
                       median: Option<f64>,
                       mad: Option<f64>,
                       dispersion: Option<f64>,
                       residual: Option<f64>,
                       middle: Option<f64>,
                       drift: Option<f64>| BeatEventIntervalRefinement {
        selected_bpm: None,
        selected_period_micros: median,
        relation: relation.to_owned(),
        usable_events,
        usable_intervals,
        interval_median_micros: median,
        interval_mad_micros: mad,
        relative_dispersion: dispersion,
        fit_residual_micros: residual,
        middle_period_micros: middle,
        early_late_drift: drift,
        available: false,
        reason: Some(reason.into()),
    };
    let Some(decoded) = wotoha_core::beat_analysis::decode_neural_rhythm(observations) else {
        return unavailable(
            "neural_event_decode_unavailable",
            0,
            0,
            None,
            None,
            None,
            None,
            None,
            None,
        );
    };
    let events = decoded.beat_events;
    if events.len() < 6 {
        return unavailable(
            "insufficient_events",
            events.len(),
            events.len().saturating_sub(1),
            None,
            None,
            None,
            None,
            None,
            None,
        );
    }
    let interval_records = events
        .windows(2)
        .enumerate()
        .filter_map(|(index, window)| {
            let left = duration_micros(window[0].time);
            let right = duration_micros(window[1].time);
            right
                .checked_sub(left)
                .filter(|value| *value > 0)
                .map(|value| (index, value as f64))
        })
        .collect::<Vec<_>>();
    let intervals = interval_records
        .iter()
        .map(|(_, interval)| *interval)
        .collect::<Vec<_>>();
    let Some(median) = median_f64(&intervals) else {
        return unavailable(
            "no_positive_intervals",
            events.len(),
            0,
            None,
            None,
            None,
            None,
            None,
            None,
        );
    };
    let absolute_deviations = intervals
        .iter()
        .map(|interval| (interval - median).abs())
        .collect::<Vec<_>>();
    let mad = median_f64(&absolute_deviations).unwrap_or_default();
    let dispersion = mad / median.max(1.0);
    let trim_limit = (median * 0.08).max(mad * 3.0).max(1.0);
    let retained = interval_records
        .iter()
        .filter(|(_, interval)| (*interval - median).abs() <= trim_limit)
        .map(|(_, interval)| *interval)
        .collect::<Vec<_>>();
    let Some(_) = median_f64(&retained) else {
        return unavailable(
            "all_intervals_rejected",
            events.len(),
            0,
            Some(median),
            Some(mad),
            Some(dispersion),
            None,
            None,
            None,
        );
    };
    if retained.len() < 5 {
        return unavailable(
            "insufficient_robust_intervals",
            events.len(),
            retained.len(),
            Some(median),
            Some(mad),
            Some(dispersion),
            None,
            None,
            None,
        );
    }
    // Use bounded multi-event spans in addition to adjacent intervals. This
    // recovers a fractional average period from frame-quantized events such
    // as 24/25/24/25 frames, while rejecting isolated large gaps and double
    // hits through the same median/MAD band.
    let maximum_span = events.len().saturating_sub(1).min(8);
    let mut span_estimates = Vec::new();
    for span in 1..=maximum_span {
        for start in 0..events.len().saturating_sub(span) {
            let left = duration_micros(events[start].time) as f64;
            let right = duration_micros(events[start + span].time) as f64;
            let estimate = (right - left) / span as f64;
            if estimate.is_finite() && estimate > 0.0 {
                let distance = (estimate - median).abs();
                if distance <= trim_limit {
                    span_estimates.push(estimate);
                }
            }
        }
    }
    if span_estimates.len() < 5 {
        return unavailable(
            "insufficient_robust_spans",
            events.len(),
            retained.len(),
            Some(median),
            Some(mad),
            Some(dispersion),
            None,
            None,
            None,
        );
    }
    let fitted_period = median_f64(&span_estimates).expect("non-empty robust spans");
    let fit_residual = span_estimates
        .iter()
        .map(|interval| (interval - fitted_period).abs())
        .sum::<f64>()
        / span_estimates.len() as f64;
    let split = intervals.len() / 2;
    let early_period = median_f64(&intervals[..split.max(1)]);
    let middle_start = intervals.len() / 3;
    let middle_end = (intervals.len() * 2 / 3).max(middle_start + 1);
    let middle_period = median_f64(&intervals[middle_start..middle_end.min(intervals.len())]);
    let late_period = median_f64(&intervals[split.max(1)..]);
    let drift = early_period
        .zip(late_period)
        .map(|(early, late)| (late - early).abs() / fitted_period.max(1.0));
    if drift.is_some_and(|value| value > 0.05) {
        return unavailable(
            "interval_drift_exceeds_global_fit",
            events.len(),
            retained.len(),
            Some(fitted_period),
            Some(mad),
            Some(dispersion),
            Some(fit_residual),
            middle_period,
            drift,
        );
    }
    let bpm = 60_000_000.0 / fitted_period;
    if !(f64::from(wotoha_core::beat_analysis::MIN_BPM)
        ..=f64::from(wotoha_core::beat_analysis::MAX_BPM))
        .contains(&bpm)
    {
        return unavailable(
            "event_tempo_out_of_range",
            events.len(),
            retained.len(),
            Some(fitted_period),
            Some(mad),
            Some(dispersion),
            Some(fit_residual),
            middle_period,
            drift,
        );
    }
    BeatEventIntervalRefinement {
        selected_bpm: Some(bpm as f32),
        selected_period_micros: Some(fitted_period),
        relation: relation.to_owned(),
        usable_events: events.len(),
        usable_intervals: retained.len(),
        interval_median_micros: Some(fitted_period),
        interval_mad_micros: Some(mad),
        relative_dispersion: Some(dispersion),
        fit_residual_micros: Some(fit_residual),
        middle_period_micros: middle_period,
        early_late_drift: drift,
        available: true,
        reason: None,
    }
}

#[derive(Clone, Debug)]
struct TempoAdvisorRow {
    fixture: TempoAdvisorFixture,
    pcm_group: String,
    lineage_group: String,
    leakage_group: String,
    candidate_label: String,
    candidate_bpm: Option<f32>,
    diagnostic_score: f64,
}

fn canonical_tempo(bpm: Option<f32>, truth: f32) -> bool {
    bpm.is_some_and(|value| relation_to_truth(Some(value), truth, false) == "primary")
}

fn tempo_metric_summary(values: &[(String, Option<f32>, f32)]) -> TempoAdvisorMetricSummary {
    let mut errors = Vec::new();
    let mut correct = 0;
    let mut half = 0;
    let mut double = 0;
    let mut other = 0;
    let mut absent = 0;
    for (_, bpm, truth) in values {
        if let Some(bpm) = bpm {
            errors.push(f64::from((*bpm - *truth).abs()));
            match relation_to_truth(Some(*bpm), *truth, false).as_str() {
                "primary" => correct += 1,
                "half_time" => half += 1,
                "double_time" => double += 1,
                _ => other += 1,
            }
        } else {
            absent += 1;
        }
    }
    let scored = values.len();
    TempoAdvisorMetricSummary {
        scored,
        canonical_correct: correct,
        canonical_accuracy: (scored > 0).then_some(correct as f64 / scored as f64),
        half_time: half,
        double_time: double,
        other_wrong: other,
        absent,
        absolute_bpm_mae: mean(&errors),
        absolute_bpm_median: percentile(&errors, 0.50),
        absolute_bpm_p95: percentile(&errors, 0.95),
    }
}

fn tempo_metric_summary_available(
    values: &[(String, Option<f32>, f32)],
) -> TempoAdvisorMetricSummary {
    let available = values
        .iter()
        .filter(|(_, bpm, _)| bpm.is_some())
        .cloned()
        .collect::<Vec<_>>();
    tempo_metric_summary(&available)
}

fn classical_tempo_margin(analysis: &NormalizedAnalysis) -> f64 {
    let mut weights = analysis
        .tempo_hypotheses
        .iter()
        .map(|hypothesis| f64::from(hypothesis.relative_weight))
        .collect::<Vec<_>>();
    weights.sort_by(f64::total_cmp);
    match (weights.last(), weights.iter().rev().nth(1)) {
        (Some(best), Some(second)) => (best - second).max(0.0),
        (Some(_), None) => 1.0,
        _ => 0.0,
    }
}

fn event_refinement_quality(refinement: Option<&BeatEventIntervalRefinement>) -> f64 {
    refinement
        .filter(|refinement| refinement.available)
        .map(|refinement| {
            let dispersion = refinement.relative_dispersion.unwrap_or(1.0);
            let residual = refinement
                .fit_residual_micros
                .zip(refinement.selected_period_micros)
                .map(|(residual, period)| residual / period.max(1.0))
                .unwrap_or(1.0);
            let drift = refinement.early_late_drift.unwrap_or(1.0);
            (1.0 - 3.0 * dispersion - 3.0 * residual - drift).clamp(0.0, 1.0)
        })
        .unwrap_or(0.0)
}

fn activation_refinement_quality(refinement: Option<&FractionalTempoRefinement>) -> f64 {
    refinement
        .map(|refinement| {
            f64::from(
                (0.45 * refinement.score
                    + 0.25 * refinement.coverage
                    + 0.20 * refinement.periodic_consistency
                    + 0.10 * refinement.phase_stability)
                    .clamp(0.0, 1.0),
            )
        })
        .unwrap_or(0.0)
}

fn choose_refined_candidate(
    current_bpm: Option<f32>,
    activation: Option<&FractionalTempoRefinement>,
    event: Option<&BeatEventIntervalRefinement>,
) -> (String, Option<f32>) {
    let activation_bpm = activation.and_then(|value| value.selected_bpm);
    let event_bpm = event.and_then(|value| value.selected_bpm);
    if let (Some(activation_bpm), Some(event_bpm)) = (activation_bpm, event_bpm) {
        let disagreement = (activation_bpm - event_bpm).abs() / activation_bpm.max(1.0);
        if disagreement <= 0.005 {
            return (
                "consensus_refined".into(),
                Some((activation_bpm + event_bpm) / 2.0),
            );
        }
        if event_refinement_quality(event) > activation_refinement_quality(activation) {
            return ("event_refined".into(), Some(event_bpm));
        }
        return ("activation_refined".into(), Some(activation_bpm));
    }
    if let Some(event_bpm) = event_bpm {
        return ("event_refined".into(), Some(event_bpm));
    }
    if let Some(activation_bpm) = activation_bpm {
        return ("activation_refined".into(), Some(activation_bpm));
    }
    ("current_neural".into(), current_bpm)
}

fn advisor_features(
    track: &TrackEvaluation,
    activation: Option<&FractionalTempoRefinement>,
    event: Option<&BeatEventIntervalRefinement>,
) -> (BTreeMap<String, f64>, f64) {
    let diagnostics = track.neural_diagnostics.as_ref();
    let activation_score = activation_refinement_quality(activation);
    let event_score = event_refinement_quality(event);
    let activation_bpm = activation.and_then(|value| value.selected_bpm);
    let event_bpm = event.and_then(|value| value.selected_bpm);
    let disagreement = activation_bpm
        .zip(event_bpm)
        .map(|(left, right)| f64::from((left - right).abs() / left.max(1.0)))
        .unwrap_or(1.0);
    let path_coverage = diagnostics
        .and_then(|value| value.path_coverage)
        .unwrap_or_default();
    let support = diagnostics
        .and_then(|value| value.support)
        .unwrap_or_default();
    let interval_residual = diagnostics
        .and_then(|value| value.interval_residual)
        .unwrap_or(1.0);
    let values = [
        ("path_coverage", f64::from(path_coverage)),
        ("support", f64::from(support)),
        ("interval_residual", f64::from(interval_residual)),
        (
            "alias_margin",
            f64::from(
                diagnostics
                    .and_then(|value| value.alias_margin)
                    .unwrap_or_default(),
            ),
        ),
        ("activation_refinement_quality", activation_score),
        ("event_refinement_quality", event_score),
        ("activation_event_disagreement", disagreement.min(1.0)),
        (
            "classical_tempo_margin",
            classical_tempo_margin(&track.wotoha),
        ),
        (
            "neural_path_marker_count",
            diagnostics.map_or(0.0, |value| value.path_marker_count as f64),
        ),
    ];
    let mut features = BTreeMap::new();
    for (name, value) in values {
        features.insert(name.into(), value);
    }
    let score = (0.24 * activation_score
        + 0.24 * event_score
        + 0.16 * features["path_coverage"].clamp(0.0, 1.0)
        + 0.14 * features["support"].clamp(0.0, 1.0)
        + 0.10 * features["alias_margin"].clamp(0.0, 1.0)
        + 0.08 * features["classical_tempo_margin"].clamp(0.0, 1.0)
        - 0.12 * disagreement
        - 0.10 * features["interval_residual"].clamp(0.0, 1.0))
    .clamp(0.0, 1.0);
    (features, score)
}

fn canonical_relation_name(relation: &str) -> &str {
    match relation {
        "halftime" | "half_time" => "half_time",
        "doubletime" | "double_time" => "double_time",
        "primary" => "primary",
        _ => relation,
    }
}

fn unavailable_tempo_candidate(bpm: f32, relation: String) -> ExperimentalTempoCandidate {
    ExperimentalTempoCandidate {
        bpm,
        relation,
        score: 0.0,
        phase_micros: 0,
        activation_support: 0.0,
        coverage: 0.0,
        periodic_consistency: 0.0,
        available: false,
    }
}

fn score_raw_periodic_candidate(
    fixture: &SyntheticFixture,
    bpm: f32,
    relation: String,
) -> Option<ExperimentalTempoCandidate> {
    let mono = downmix(&fixture.audio, fixture.spec.channels);
    let samples = if fixture.spec.sample_rate == DEFAULT_SAMPLE_RATE {
        mono
    } else {
        resample(&mono, fixture.spec.sample_rate, DEFAULT_SAMPLE_RATE)
    };
    let frame_size = (DEFAULT_SAMPLE_RATE as usize / 100).max(1);
    let envelope = samples
        .chunks(frame_size)
        .map(|chunk| {
            (chunk.iter().map(|sample| sample * sample).sum::<f32>() / chunk.len().max(1) as f32)
                .sqrt()
        })
        .collect::<Vec<_>>();
    if envelope.len() < 8 {
        return None;
    }
    let period_frames = (6_000.0 / bpm).round() as usize;
    if period_frames == 0 || period_frames >= envelope.len() {
        return None;
    }
    let global = envelope.iter().sum::<f32>() / envelope.len() as f32;
    if !global.is_finite() || global <= f32::EPSILON {
        return None;
    }
    let mut best = None;
    for phase in 0..period_frames {
        let selected = (phase..envelope.len())
            .step_by(period_frames)
            .map(|index| {
                let from = index.saturating_sub(1);
                let to = (index + 2).min(envelope.len());
                envelope[from..to].iter().copied().fold(0.0, f32::max)
            })
            .collect::<Vec<_>>();
        if selected.len() < 3 {
            continue;
        }
        let activation_support =
            (selected.iter().sum::<f32>() / selected.len() as f32 / global).clamp(0.0, 1.0);
        let coverage = selected
            .iter()
            .filter(|value| **value >= global * 1.15)
            .count() as f32
            / selected.len() as f32;
        let paired = (phase..envelope.len().saturating_sub(period_frames))
            .map(|index| envelope[index] * envelope[index + period_frames])
            .sum::<f32>();
        let energy = envelope
            .iter()
            .map(|value| value * value)
            .sum::<f32>()
            .max(f32::EPSILON);
        let periodic_consistency = (paired / energy).clamp(0.0, 1.0);
        let score = (0.45 * activation_support + 0.30 * coverage + 0.25 * periodic_consistency)
            .clamp(0.0, 1.0);
        let candidate = ExperimentalTempoCandidate {
            bpm,
            relation: relation.clone(),
            score,
            phase_micros: phase as u64 * 10_000,
            activation_support,
            coverage,
            periodic_consistency,
            available: true,
        };
        if best
            .as_ref()
            .is_none_or(|current: &ExperimentalTempoCandidate| candidate.score > current.score)
        {
            best = Some(candidate);
        }
    }
    best
}

fn experimental_tempo_report(
    manifest: &SyntheticCorpusManifest,
    results: &BTreeMap<String, TempoResolverResults>,
    predictions: &BTreeMap<String, NormalizedAnalysis>,
) -> ExperimentalTempoReport {
    let mut report = ExperimentalTempoReport::default();
    for record in &manifest.fixtures {
        let Some(resolvers) = results.get(&record.spec.id) else {
            continue;
        };
        let experimental = &resolvers.pcm;
        let Some(prediction) = predictions.get(&record.spec.id) else {
            continue;
        };
        let Some(truth) = record.truth.tempo.as_ref() else {
            continue;
        };
        report.applicable_tracks += 1;
        let current_bpm = primary_tempo_bpm(prediction);
        let current_correct = current_bpm
            .is_some_and(|bpm| (bpm - truth.primary_bpm).abs() / truth.primary_bpm < 0.005);
        let current_relation_to_truth = relation_to_truth(current_bpm, truth.primary_bpm, false);
        let pcm_relation_to_truth = relation_to_truth(
            experimental.selected_bpm,
            truth.primary_bpm,
            experimental.ambiguous,
        );
        let experimental_correct = experimental
            .selected_bpm
            .is_some_and(|bpm| (bpm - truth.primary_bpm).abs() / truth.primary_bpm < 0.005);
        report.current_primary_correct_count += usize::from(current_correct);
        report.experimental_primary_correct_count += usize::from(experimental_correct);
        let activation_correct = resolvers.activation.as_ref().and_then(|resolution| {
            resolution
                .selected_bpm
                .map(|bpm| (bpm - truth.primary_bpm).abs() / truth.primary_bpm < 0.005)
        });
        let activation_relation_to_truth = resolvers.activation.as_ref().map(|activation| {
            relation_to_truth(
                activation.selected_bpm,
                truth.primary_bpm,
                activation.ambiguous,
            )
        });
        report.activation_primary_correct_count += usize::from(activation_correct == Some(true));
        report.current_half_time_count += usize::from(current_relation_to_truth == "half_time");
        report.current_double_time_count += usize::from(current_relation_to_truth == "double_time");
        report.experimental_ambiguous_count += usize::from(experimental.ambiguous);
        report.experimental_wrong_primary_count +=
            usize::from(!experimental.ambiguous && !experimental_correct);
        let experimental_relation = experimental
            .selected_bpm
            .and_then(|bpm| tempo_relation_to_truth(bpm, truth.primary_bpm));
        if experimental_relation == Some("half_time") {
            report.experimental_half_time_count += 1;
        }
        if experimental_relation == Some("double_time") {
            report.experimental_double_time_count += 1;
        }
        if let Some(activation) = &resolvers.activation {
            report.activation_ambiguous_count += usize::from(activation.ambiguous);
            report.activation_wrong_primary_count +=
                usize::from(!activation.ambiguous && activation_correct != Some(true));
            let activation_relation = activation
                .selected_bpm
                .and_then(|bpm| tempo_relation_to_truth(bpm, truth.primary_bpm));
            if activation_relation == Some("half_time") {
                report.activation_half_time_count += 1;
            }
            if activation_relation == Some("double_time") {
                report.activation_double_time_count += 1;
            }
        }
        if current_correct && (!experimental_correct || experimental.ambiguous) {
            report
                .pcm_regressions_vs_production
                .push(record.spec.id.clone());
        }
        if current_correct
            && resolvers
                .activation
                .as_ref()
                .is_some_and(|activation| activation.ambiguous || activation_correct != Some(true))
        {
            report
                .activation_regressions_vs_production
                .push(record.spec.id.clone());
        }
        let family = record.spec.family.as_str().to_owned();
        let family_summary = report.by_fixture_family.entry(family.clone()).or_default();
        family_summary.tracks += 1;
        family_summary.current_primary_correct_count += usize::from(current_correct);
        family_summary.experimental_primary_correct_count += usize::from(experimental_correct);
        family_summary.activation_primary_correct_count +=
            usize::from(activation_correct == Some(true));
        family_summary.experimental_ambiguous_count += usize::from(experimental.ambiguous);
        family_summary.activation_ambiguous_count += usize::from(
            resolvers
                .activation
                .as_ref()
                .is_some_and(|activation| activation.ambiguous),
        );
        report.per_fixture.push(ExperimentalTempoFixture {
            sample_id: record.spec.id.clone(),
            family,
            truth_bpm: Some(truth.primary_bpm),
            current_primary_bpm: current_bpm,
            current_relation_to_truth,
            pcm_relation_to_truth,
            activation_relation_to_truth,
            pcm: experimental.clone(),
            activation: resolvers.activation.clone(),
            fractional_refinement: None,
            raw_observations: resolvers.raw_observations.clone(),
        });
    }
    report
}

fn production_selection_relation(track: &TrackEvaluation) -> String {
    track
        .neural_diagnostics
        .as_ref()
        .and_then(|diagnostics| {
            diagnostics.selected_period_frames.and_then(|period| {
                diagnostics
                    .candidates
                    .iter()
                    .find(|candidate| candidate.available && candidate.period_frames == period)
                    .map(|candidate| canonical_relation_name(&candidate.relation).to_owned())
            })
        })
        .unwrap_or_else(|| "unknown".into())
}

fn tempo_refinement_primary_cohort(
    fixture: &ExperimentalTempoFixture,
    track: &TrackEvaluation,
) -> (bool, Option<String>) {
    if track.analysis_backend == "native_neural"
        && fixture.raw_observations.is_some()
        && fixture.current_primary_bpm.is_some()
    {
        (true, None)
    } else if track.analysis_backend == "classical_fallback" {
        (false, Some("classical_fallback".into()))
    } else if fixture.raw_observations.is_none() {
        (false, Some("raw_neural_observations_absent".into()))
    } else if fixture.current_primary_bpm.is_none() {
        (false, Some("production_neural_bpm_absent".into()))
    } else {
        (false, Some("not_native_neural".into()))
    }
}

fn build_tempo_refinement_report(
    report: &EvaluationReport,
    source_commit: Option<String>,
) -> TempoRefinementReport {
    let mut cases = Vec::new();
    let mut current_errors = Vec::new();
    let mut refined_errors = Vec::new();
    let mut current_correct = 0;
    let mut refined_correct = 0;
    let mut current_half = 0;
    let mut refined_half = 0;
    let mut current_double = 0;
    let mut refined_double = 0;
    let mut current_other_wrong = 0;
    let mut refined_other_wrong = 0;
    let mut fallback_excluded_count = 0;
    let mut by_fixture_family = BTreeMap::<String, TempoRefinementGroupSummary>::new();
    let per_track = report
        .per_track
        .iter()
        .map(|track| (track.sample_id.as_str(), track))
        .collect::<BTreeMap<_, _>>();
    for fixture in &report.tempo_experiment.per_fixture {
        let Some(truth_bpm) = fixture.truth_bpm else {
            continue;
        };
        let Some(track) = per_track.get(fixture.sample_id.as_str()) else {
            continue;
        };
        let current_backend = track.analysis_backend.clone();
        let (primary_cohort, exclusion_reason) = tempo_refinement_primary_cohort(fixture, track);
        fallback_excluded_count +=
            usize::from(exclusion_reason.as_deref() == Some("classical_fallback"));
        let selection_relation_before = if primary_cohort {
            production_selection_relation(track)
        } else {
            "unknown".into()
        };
        let current_error = fixture
            .current_primary_bpm
            .map(|bpm| (bpm - truth_bpm).abs());
        let truth_relation_before =
            relation_to_truth(fixture.current_primary_bpm, truth_bpm, false);
        let fractional_refinement = if primary_cohort {
            fixture.raw_observations.as_ref().and_then(|observations| {
                fractional_tempo_refinement(
                    observations,
                    fixture.current_primary_bpm,
                    &selection_relation_before,
                )
            })
        } else {
            None
        };
        let refined_bpm = fractional_refinement
            .as_ref()
            .and_then(|refinement| refinement.selected_bpm);
        let refined_error = refined_bpm.map(|bpm| (bpm - truth_bpm).abs());
        let truth_relation_after = if primary_cohort {
            relation_to_truth(refined_bpm, truth_bpm, false)
        } else {
            "not_evaluated".into()
        };
        let family_summary = by_fixture_family.entry(fixture.family.clone()).or_default();
        if primary_cohort {
            family_summary.tracks += 1;
            if let Some(error) = current_error {
                current_errors.push(f64::from(error));
            }
            if let Some(error) = refined_error {
                refined_errors.push(f64::from(error));
            }
            current_correct += usize::from(truth_relation_before == "primary");
            refined_correct += usize::from(truth_relation_after == "primary");
            current_half += usize::from(truth_relation_before == "half_time");
            refined_half += usize::from(truth_relation_after == "half_time");
            current_double += usize::from(truth_relation_before == "double_time");
            refined_double += usize::from(truth_relation_after == "double_time");
            current_other_wrong += usize::from(
                truth_relation_before == "other_wrong" || truth_relation_before == "absent",
            );
            refined_other_wrong += usize::from(
                truth_relation_after == "other_wrong" || truth_relation_after == "absent",
            );
            family_summary.current_correct += usize::from(truth_relation_before == "primary");
            family_summary.refined_correct += usize::from(truth_relation_after == "primary");
            family_summary.half_time_count += usize::from(truth_relation_after == "half_time");
            family_summary.double_time_count += usize::from(truth_relation_after == "double_time");
            family_summary.other_wrong_count += usize::from(
                truth_relation_after == "other_wrong" || truth_relation_after == "absent",
            );
        } else {
            family_summary.excluded_tracks += 1;
        }
        cases.push(TempoRefinementCase {
            sample_id: fixture.sample_id.clone(),
            family: fixture.family.clone(),
            current_backend,
            primary_cohort,
            exclusion_reason,
            truth_bpm,
            current_selected_bpm: fixture.current_primary_bpm,
            refined_bpm,
            current_absolute_error_bpm: current_error,
            refined_absolute_error_bpm: refined_error,
            selection_relation_after: selection_relation_before.clone(),
            selection_relation_before,
            truth_relation_before,
            truth_relation_after,
            refinement: fractional_refinement,
        });
    }
    for (family, summary) in &mut by_fixture_family {
        let family_cases = cases
            .iter()
            .filter(|case| case.primary_cohort && case.family == *family)
            .collect::<Vec<_>>();
        summary.current_mean_absolute_error_bpm = mean(
            &family_cases
                .iter()
                .filter_map(|case| case.current_absolute_error_bpm.map(f64::from))
                .collect::<Vec<_>>(),
        );
        summary.refined_mean_absolute_error_bpm = mean(
            &family_cases
                .iter()
                .filter_map(|case| case.refined_absolute_error_bpm.map(f64::from))
                .collect::<Vec<_>>(),
        );
    }
    cases.sort_by(|left, right| left.sample_id.cmp(&right.sample_id));
    let required_ids = [
        "constant-120.0",
        "constant-127.5",
        "constant-128.0",
        "constant-130.0",
        "constant-140.0",
        "constant-150.0",
        "constant-160.0",
        "constant-180.0",
        "half-double-64-128",
        "half-double-70-140",
        "half-double-75-150",
        "half-double-80-160",
        "half-double-85-170",
        "half-double-90-180",
    ];
    let required_cases = required_ids
        .into_iter()
        .filter_map(|id| {
            cases
                .iter()
                .find(|case| case.sample_id == id)
                .cloned()
                .map(|case| (id.into(), case))
        })
        .collect();
    let resolution = cases
        .iter()
        .filter_map(|case| {
            case.refinement
                .as_ref()
                .map(|refinement| refinement.resolution_frames)
        })
        .next()
        .unwrap_or(0.1);
    TempoRefinementReport {
        schema_version: RESEARCH_REPORT_SCHEMA_VERSION,
        evaluator: format!("wotoha-analysis-lab/{}", env!("CARGO_PKG_VERSION")),
        split: report.split.clone(),
        source_commit,
        algorithm: "local fractional period and phase search over raw Beat This activation logits; current octave relation preserved".into(),
        fractional_resolution_frames: resolution,
        primary_cohort_size: current_errors.len(),
        fallback_excluded_count,
        current_neural_mean_absolute_error_bpm: mean(&current_errors),
        refined_mean_absolute_error_bpm: mean(&refined_errors),
        current_median_absolute_error_bpm: percentile(&current_errors, 0.50),
        current_p95_absolute_error_bpm: percentile(&current_errors, 0.95),
        refined_median_absolute_error_bpm: percentile(&refined_errors, 0.50),
        refined_p95_absolute_error_bpm: percentile(&refined_errors, 0.95),
        current_canonical_correctness: current_correct,
        refined_canonical_correctness: refined_correct,
        current_half_time_count: current_half,
        refined_half_time_count: refined_half,
        current_double_time_count: current_double,
        refined_double_time_count: refined_double,
        current_other_wrong_count: current_other_wrong,
        refined_other_wrong_count: refined_other_wrong,
        by_fixture_family,
        required_cases,
        per_fixture: cases,
        variable_tempo_excluded_from_global_bpm: true,
        beat_events_changed: false,
        production_recommendation: "do-not-promote: label-only refinement; require held-out regression evidence before any production proposal".into(),
    }
}

fn relation_to_truth(bpm: Option<f32>, truth_bpm: f32, ambiguous: bool) -> String {
    if ambiguous {
        return "ambiguous".into();
    }
    bpm.and_then(|bpm| tempo_relation_to_truth(bpm, truth_bpm))
        .unwrap_or(if bpm.is_some() {
            "other_wrong"
        } else {
            "absent"
        })
        .into()
}

fn tempo_relation_to_truth(bpm: f32, truth_bpm: f32) -> Option<&'static str> {
    let within = |expected: f32| expected > 0.0 && (bpm - expected).abs() / expected < 0.005;
    if within(truth_bpm) {
        Some("primary")
    } else if within(truth_bpm / 2.0) {
        Some("half_time")
    } else if within(truth_bpm * 2.0) {
        Some("double_time")
    } else {
        None
    }
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
    identities: Option<&BTreeMap<String, ExternalIdentity>>,
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
        if let Some(identity) = identities.and_then(|items| items.get(&observation.sample_id)) {
            verify_external_observation_identity(observation, identity)?;
        } else if record.audio_sha256 != observation.audio_sha256 {
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
        let platform = observation
            .observer
            .platform
            .clone()
            .unwrap_or_else(|| "unknown".into());
        let settings = observation_settings_key(&observation.analysis_settings);
        let key = format!(
            "{}@{} [{}] [{}]",
            observation.observer.product, observation.observer.version, platform, settings
        );
        let entry = by_observer
            .entry(key)
            .or_insert_with(|| ExternalObserverReport {
                product: observation.observer.product.clone(),
                version: observation.observer.version.clone(),
                platform: platform.clone(),
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

fn verify_external_observation_identity(
    observation: &ExternalAnalysisObservation,
    identity: &ExternalIdentity,
) -> Result<(), LabError> {
    if observation.audio_sha256 != identity.wav_file_sha256 {
        return Err(LabError::HashMismatch {
            sample_id: observation.sample_id.clone(),
            expected: identity.wav_file_sha256.clone(),
            actual: observation.audio_sha256.clone(),
        });
    }
    if let Some(pcm_sha256) = observation.pcm_sha256.as_deref()
        && pcm_sha256 != identity.pcm_sha256
    {
        return Err(LabError::HashMismatch {
            sample_id: observation.sample_id.clone(),
            expected: identity.pcm_sha256.clone(),
            actual: pcm_sha256.to_owned(),
        });
    }
    if let Some(ground_truth_sha256) = observation.ground_truth_sha256.as_deref()
        && ground_truth_sha256 != identity.ground_truth_sha256
    {
        return Err(LabError::HashMismatch {
            sample_id: observation.sample_id.clone(),
            expected: identity.ground_truth_sha256.clone(),
            actual: ground_truth_sha256.to_owned(),
        });
    }
    Ok(())
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
            unobserved_beat_metrics()
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
    let phase_reference = external_phase_reference(
        beats_available.then_some(external_beats.as_deref().unwrap_or_default()),
        external.reported_bpm,
        external.grid_phase_micros,
    );
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
            unobserved_beat_metrics()
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
        grid_phase: match (wotoha.beats.first(), phase_reference) {
            (Some(predicted), Some((reference, period))) if period > 0 => {
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
            audio_file: None,
            audio_sha256: "a".repeat(64),
            pcm_sha256: None,
            ground_truth_sha256: None,
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
    fn meter_clear_fixtures_have_distinct_truth_and_accents() {
        let manifest = generate_default_manifest(42).unwrap();
        let mut hashes = BTreeSet::new();
        for (id, meter) in [
            ("meter-clear-2-4", 2_usize),
            ("meter-clear-3-4", 3),
            ("meter-clear-4-4", 4),
            ("meter-clear-6-8", 6),
        ] {
            let record = manifest
                .fixtures
                .iter()
                .find(|fixture| fixture.spec.id == id)
                .unwrap();
            assert_eq!(record.truth.meter, Some(meter as u8));
            assert_eq!(
                record.truth.downbeats,
                (0..record.truth.beat_times_micros.len())
                    .step_by(meter)
                    .collect::<Vec<_>>()
            );
            assert!(hashes.insert(record.audio_sha256.clone()));
            let fixture = generate_fixture(&record.spec).unwrap();
            let energy = |beat_index: usize| {
                let center = record.truth.beat_times_micros[beat_index] as usize
                    * fixture.spec.sample_rate as usize
                    / 1_000_000;
                let radius = fixture.spec.sample_rate as usize / 100;
                fixture.audio
                    [center.saturating_sub(radius)..=(center + radius).min(fixture.audio.len() - 1)]
                    .iter()
                    .map(|sample| sample.abs())
                    .fold(0.0, f32::max)
            };
            assert!(energy(0) > energy(1));
            if meter == 4 || meter == 6 {
                assert!(energy(meter / 2) > energy(1));
            }
            let expected = match meter {
                2 | 3 => vec![
                    meter_accent(meter as u8, 0),
                    meter_accent(meter as u8, 1),
                    meter_accent(meter as u8, 2),
                ],
                4 => (0..4).map(|beat| meter_accent(meter as u8, beat)).collect(),
                6 => (0..6).map(|beat| meter_accent(meter as u8, beat)).collect(),
                _ => unreachable!(),
            };
            assert_eq!(expected[0], (0.95, 0.85, 0.75));
            if meter == 4 || meter == 6 {
                assert_eq!(expected[meter / 2], (0.62, 0.55, 0.60));
            }
            let weak = if meter == 2 || meter == 3 {
                (0.30, 0.25, 0.45)
            } else {
                (0.28, 0.24, 0.42)
            };
            for (beat, accent) in expected.iter().enumerate() {
                if beat != 0 && beat != meter / 2 {
                    assert_eq!(*accent, weak);
                }
            }
        }
        let ambiguous = manifest
            .fixtures
            .iter()
            .find(|fixture| fixture.spec.id == "meter-ambiguous-4-4")
            .unwrap();
        assert_eq!(ambiguous.truth.meter, None);
    }

    #[test]
    fn meter_truth_is_explicit_and_id_renaming_cannot_change_unknown() {
        let manifest = generate_default_manifest(42).unwrap();
        let original = manifest
            .fixtures
            .iter()
            .find(|fixture| fixture.spec.id == "meter-ambiguous-4-4")
            .unwrap();
        let mut renamed = original.spec.clone();
        renamed.id = "unrelated-stimulus-name".into();
        assert_eq!(original.truth.meter, None);
        assert_eq!(generate_fixture(&renamed).unwrap().truth.meter, None);
        assert_eq!(renamed.meter, 4);
        assert!(renamed.meter_truth.is_none());
    }

    #[test]
    fn meter_truth_accepts_only_explicit_supported_values_and_schema_is_versioned() {
        let mut manifest = generate_default_manifest(42).unwrap();
        for meter in [2_u8, 3, 4, 6] {
            let mut fixture = manifest.fixtures[0].spec.clone();
            fixture.id = format!("meter-truth-{meter}");
            fixture.meter = meter;
            fixture.meter_truth = Some(meter);
            assert!(generate_fixture(&fixture).is_ok());
        }
        let mut invalid = manifest.fixtures[0].spec.clone();
        invalid.meter_truth = Some(5);
        assert!(generate_fixture(&invalid).is_err());
        manifest.schema_version = LAB_SCHEMA_VERSION - 1;
        assert!(manifest.validate().is_err());
    }

    #[test]
    fn half_double_corpus_covers_requested_tempo_families() {
        let manifest = generate_default_manifest(42).unwrap();
        for (id, bpm) in [
            ("half-double-64-128", 128.0),
            ("half-double-70-140", 140.0),
            ("half-double-75-150", 150.0),
            ("half-double-80-160", 160.0),
            ("half-double-85-170", 170.0),
            ("half-double-90-180", 180.0),
        ] {
            let fixture = manifest
                .fixtures
                .iter()
                .find(|fixture| fixture.spec.id == id)
                .unwrap();
            let truth = fixture.truth.tempo.as_ref().unwrap();
            assert_eq!(truth.primary_bpm, bpm);
            assert!(truth.valid_alternates_bpm.contains(&(bpm / 2.0)));
            assert!(truth.valid_alternates_bpm.contains(&(bpm * 2.0)));
        }
    }

    #[test]
    fn meter_evidence_reports_all_candidate_slots_and_absence() {
        let mut prediction = normalized_prediction(&[0, 500_000, 1_000_000]);
        prediction.meter_hypotheses = vec![NormalizedMeterHypothesis {
            beats_per_bar: 4,
            downbeat_phase: 2,
            score: 0.8,
        }];
        let evidence = meter_evidence(&prediction);
        assert_eq!(evidence.candidates.len(), 4);
        assert_eq!(evidence.candidates["2"].score, None);
        assert_eq!(evidence.candidates["3"].phase, None);
        assert_eq!(evidence.candidates["4"].score, Some(0.8));
        assert_eq!(evidence.candidates["4"].phase, Some(2));
        assert_eq!(evidence.candidates["6"].score, None);
    }

    #[test]
    fn blackbox_export_is_deterministic_and_round_trips_exported_audio() {
        let root =
            std::env::temp_dir().join(format!("wotoha-analysis-lab-export-{}", std::process::id()));
        let repeat = root.with_extension("repeat");
        let zip = root.with_extension("zip");
        let repeat_zip = repeat.with_extension("zip");
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&repeat);
        let _ = fs::remove_file(&zip);
        let _ = fs::remove_file(&repeat_zip);
        let first = export_blackbox(&root, 17).unwrap();
        let second = export_blackbox(&repeat, 17).unwrap();
        assert_eq!(first.fixtures.len(), second.fixtures.len());
        assert_eq!(
            fs::read(root.join("manifest.json")).unwrap(),
            fs::read(repeat.join("manifest.json")).unwrap()
        );
        let manifest_json =
            String::from_utf8(fs::read(root.join("manifest.json")).unwrap()).unwrap();
        assert!(manifest_json.contains("\"wav_file_sha256\""));
        assert!(manifest_json.contains("\"generated_float_fixture_sha256\""));
        assert!(!manifest_json.contains("\"audio_sha256\""));
        let first_fixture = first.fixtures.first().unwrap();
        let wav = fs::read(root.join(&first_fixture.audio_file)).unwrap();
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        let decoded = decode_wav_pcm16(&wav).unwrap();
        assert_eq!(decoded.sample_rate, first_fixture.sample_rate);
        assert_eq!(decoded.channels, first_fixture.channels);
        assert_eq!(
            decoded.frame_count,
            first_fixture.sample_count / usize::from(decoded.channels)
        );
        assert_eq!(decoded.pcm_sha256, first_fixture.pcm_sha256);
        let template: ExternalObservationDocument = serde_json::from_slice(
            &fs::read(root.join("external-observations-template.json")).unwrap(),
        )
        .unwrap();
        let template_json =
            String::from_utf8(fs::read(root.join("external-observations-template.json")).unwrap())
                .unwrap();
        assert!(template_json.contains("\"wav_file_sha256\""));
        assert!(!template_json.contains("\"audio_sha256\""));
        assert_eq!(template.observations.len(), first.fixtures.len());
        assert_eq!(
            template
                .observations
                .iter()
                .map(|observation| observation.sample_id.clone())
                .collect::<BTreeSet<_>>(),
            first
                .fixtures
                .iter()
                .map(|fixture| fixture.sample_id.clone())
                .collect::<BTreeSet<_>>()
        );
        let mut ids = BTreeSet::new();
        for fixture in &first.fixtures {
            assert!(ids.insert(fixture.sample_id.clone()));
            assert_eq!(fixture.audio_sha256.len(), 64);
            assert_eq!(fixture.pcm_sha256.len(), 64);
            let bytes = fs::read(root.join(&fixture.audio_file)).unwrap();
            assert_eq!(hash_bytes(&bytes), fixture.audio_sha256);
        }
        let report = evaluate_exported_manifest(
            &root.join("manifest.json"),
            &root,
            None,
            EvaluationOptions {
                mode: AnalyzerMode::Classical,
                split: "development".into(),
                source_commit: None,
                include_backend_comparison: false,
            },
        )
        .unwrap();
        assert_eq!(report.overall.tracks, first.fixtures.len());
        package_blackbox(&root, &zip).unwrap();
        package_blackbox(&repeat, &repeat_zip).unwrap();
        assert_eq!(fs::read(&zip).unwrap(), fs::read(&repeat_zip).unwrap());
        verify_blackbox_package(&zip).unwrap();
        let mut corrupt = fs::read(&zip).unwrap();
        let corruption_index = corrupt.len() / 2;
        corrupt[corruption_index] ^= 1;
        fs::write(&repeat_zip, corrupt).unwrap();
        assert!(verify_blackbox_package(&repeat_zip).is_err());
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&repeat);
        let _ = fs::remove_file(&zip);
        let _ = fs::remove_file(&repeat_zip);
    }

    fn test_stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut archive = Vec::new();
        let mut central = Vec::new();
        for (name, data) in entries {
            let offset = archive.len() as u32;
            let name = name.as_bytes();
            let size = data.len() as u32;
            let crc = crc32(data);
            write_zip_local_header(&mut archive, name, crc, size);
            archive.extend_from_slice(data);
            write_zip_central_header(&mut central, name, crc, size, offset);
        }
        let central_offset = archive.len() as u32;
        let central_size = central.len() as u32;
        archive.extend_from_slice(&central);
        let count = entries.len() as u16;
        write_u32(&mut archive, 0x0605_4b50);
        write_u16(&mut archive, 0);
        write_u16(&mut archive, 0);
        write_u16(&mut archive, count);
        write_u16(&mut archive, count);
        write_u32(&mut archive, central_size);
        write_u32(&mut archive, central_offset);
        write_u16(&mut archive, 0);
        archive
    }

    #[test]
    fn blackbox_zip_rejects_unsafe_duplicate_and_oversized_entries() {
        assert!(read_stored_zip(&test_stored_zip(&[("../escape", b"x")])).is_err());
        assert!(read_stored_zip(&test_stored_zip(&[("../../escape", b"x")])).is_err());
        assert!(read_stored_zip(&test_stored_zip(&[("/absolute/path", b"x")])).is_err());
        assert!(read_stored_zip(&test_stored_zip(&[("C:/absolute/path", b"x")])).is_err());
        assert!(read_stored_zip(&test_stored_zip(&[("C:\\absolute\\path", b"x")])).is_err());
        assert!(
            read_stored_zip(&test_stored_zip(&[
                ("audio/a.wav", b"x"),
                ("audio/a.wav", b"x")
            ]))
            .is_err()
        );

        let mut oversized = test_stored_zip(&[("audio/a.wav", b"x")]);
        let central = oversized
            .windows(4)
            .position(|window| window == 0x0201_4b50u32.to_le_bytes())
            .expect("central directory");
        let too_large = (MAX_PACKAGE_FILE_BYTES as u32).saturating_add(1);
        oversized[central + 20..central + 24].copy_from_slice(&too_large.to_le_bytes());
        oversized[central + 24..central + 28].copy_from_slice(&too_large.to_le_bytes());
        assert!(read_stored_zip(&oversized).is_err());
    }

    #[test]
    fn neural_diagnostics_are_observational_and_bounded() {
        let mut beat_logits = vec![-6.0; 400];
        let downbeat_logits = vec![-6.0; 400];
        for frame in (10..390).step_by(25) {
            beat_logits[frame] = 7.0;
        }
        let observations = wotoha_core::beat_analysis::NeuralBeatObservations::with_frame_rate(
            50.0,
            beat_logits,
            downbeat_logits,
        )
        .unwrap();
        let normal = wotoha_core::beat_analysis::decode_neural_rhythm(&observations);
        let (diagnostic, evidence) =
            wotoha_core::beat_analysis::diagnose_neural_rhythm(&observations, &[]);
        assert_eq!(diagnostic, normal);
        assert!(evidence.decoder_accepted);
        assert!(
            evidence
                .candidates
                .iter()
                .all(|candidate| candidate.bpm.is_finite()
                    && candidate.activation_evidence.is_finite()
                    && candidate.coverage.is_finite()
                    && candidate.off_grid_leakage.is_finite()
                    && candidate.periodic_consistency.is_finite()
                    && candidate.candidate_score.is_finite())
        );
        assert!(evidence.candidates.iter().any(|candidate| {
            candidate.relation == wotoha_core::beat_analysis::TempoRelation::HalfTime
                && candidate.available
        }));
        assert!(evidence.candidates.iter().any(|candidate| {
            candidate.relation == wotoha_core::beat_analysis::TempoRelation::DoubleTime
                && !candidate.available
        }));
        let available = evidence
            .candidates
            .iter()
            .filter(|candidate| candidate.available)
            .collect::<Vec<_>>();
        assert!(
            available
                .windows(2)
                .any(|pair| pair[0].candidate_score != pair[1].candidate_score)
        );
    }

    #[test]
    fn neural_diagnostics_preserve_rejection_reasons() {
        let insufficient = wotoha_core::beat_analysis::NeuralBeatObservations {
            frame_rate_hz: 50.0,
            beat_logits: vec![0.0; 2],
            downbeat_logits: vec![0.0; 2],
        };
        let (decoded, diagnostics) =
            wotoha_core::beat_analysis::diagnose_neural_rhythm(&insufficient, &[]);
        assert_eq!(decoded, None);
        assert_eq!(
            diagnostics.rejection_reason.as_deref(),
            Some("insufficient_markers")
        );

        let invalid = wotoha_core::beat_analysis::NeuralBeatObservations {
            frame_rate_hz: 50.0,
            beat_logits: vec![f32::NAN; 32],
            downbeat_logits: vec![0.0; 32],
        };
        let (_, diagnostics) = wotoha_core::beat_analysis::diagnose_neural_rhythm(&invalid, &[]);
        assert_eq!(
            diagnostics.rejection_reason.as_deref(),
            Some("invalid_input")
        );
    }

    #[test]
    fn experimental_tempo_resolver_reports_independent_candidate_evidence() {
        let spec = spec(
            "tempo-experiment",
            FixtureFamily::HalfDouble,
            TempoProfile::Constant { bpm: 128.0 },
            EventStyle::WeakSubdivision,
            4,
            0,
            8,
        );
        let fixture = generate_fixture(&spec).unwrap();
        let mut prediction = normalized_prediction(&[0, 500_000, 1_000_000, 1_500_000]);
        prediction.tempo_hypotheses = vec![
            NormalizedTempoHypothesis {
                bpm: 64.0,
                relative_weight: 0.3,
                relation: "half_time".into(),
            },
            NormalizedTempoHypothesis {
                bpm: 128.0,
                relative_weight: 0.5,
                relation: "primary".into(),
            },
            NormalizedTempoHypothesis {
                bpm: 256.0,
                relative_weight: 0.2,
                relation: "double_time".into(),
            },
        ];
        let result = experimental_tempo_resolution(&fixture, &prediction);
        assert!(result.candidates.len() >= 2);
        assert!(
            result
                .candidates
                .iter()
                .all(|candidate| candidate.score.is_finite())
        );
    }

    #[test]
    fn experimental_tempo_resolver_exposes_family_edges_and_unavailable_aliases() {
        let spec = spec(
            "tempo-edge",
            FixtureFamily::HalfDouble,
            TempoProfile::Constant { bpm: 180.0 },
            EventStyle::HatsOnly,
            4,
            0,
            1,
        );
        let fixture = generate_fixture(&spec).unwrap();
        let mut prediction = normalized_prediction(&[0, 333_333, 666_666]);
        prediction.tempo_hypotheses = vec![NormalizedTempoHypothesis {
            bpm: 180.0,
            relative_weight: 1.0,
            relation: "primary".into(),
        }];
        let result = experimental_tempo_resolution(&fixture, &prediction);
        let family = result
            .candidates
            .iter()
            .map(|candidate| (candidate.relation.as_str(), candidate))
            .collect::<BTreeMap<_, _>>();
        assert!(family["half_time"].available);
        assert!(family["primary"].available);
        assert!(!family["double_time"].available);
        assert_eq!(family["double_time"].score, 0.0);
        assert_eq!(result, experimental_tempo_resolution(&fixture, &prediction));
    }

    #[test]
    fn experimental_tempo_resolver_can_return_ambiguity_without_a_winner() {
        let candidates = vec![
            ExperimentalTempoCandidate {
                bpm: 64.0,
                relation: "half_time".into(),
                score: 0.80,
                phase_micros: 0,
                activation_support: 0.8,
                coverage: 0.8,
                periodic_consistency: 0.8,
                available: true,
            },
            ExperimentalTempoCandidate {
                bpm: 128.0,
                relation: "primary".into(),
                score: 0.79,
                phase_micros: 0,
                activation_support: 0.79,
                coverage: 0.79,
                periodic_consistency: 0.79,
                available: true,
            },
        ];
        let (selected, ambiguous) = choose_experimental_candidate(&candidates);
        assert!(ambiguous);
        assert_eq!(selected, None);
        assert_eq!(
            relation_to_truth(Some(128.0), 128.0, ambiguous),
            "ambiguous"
        );
        assert_eq!(relation_to_truth(Some(96.0), 128.0, false), "other_wrong");
        assert_eq!(relation_to_truth(None, 128.0, false), "absent");
    }

    #[test]
    fn activation_tempo_resolver_is_deterministic_and_has_explicit_aliases() {
        let mut beat = vec![-7.0; 900];
        for frame in (25..900).step_by(25) {
            beat[frame] = 7.0;
        }
        let observations =
            wotoha_core::beat_analysis::NeuralBeatObservations::new(beat, vec![-7.0; 900]).unwrap();
        let first = activation_tempo_resolution(&observations, Some(120.0)).unwrap();
        let second = activation_tempo_resolution(&observations, Some(120.0)).unwrap();
        assert_eq!(first, second);
        assert!(first.candidates.iter().all(|candidate| {
            candidate.candidate_score.is_finite()
                && candidate.off_grid_leakage.is_finite()
                && candidate.off_grid_leakage <= 1.0
        }));
        assert!(
            first
                .candidates
                .iter()
                .any(|candidate| candidate.relation == "half_time")
        );
        assert!(
            first
                .candidates
                .iter()
                .any(|candidate| candidate.relation == "double_time")
        );
    }

    #[test]
    fn backend_outcomes_are_dimension_aware_and_not_one_aggregate_score() {
        let snapshot = |recall: f64, valid: bool| BackendMetricSnapshot {
            beat_mae_ms: Some(if valid { 5.0 } else { 200.0 }),
            beat_p50_ms: Some(4.0),
            beat_p95_ms: Some(10.0),
            precision_at_40ms: Some(recall),
            recall_at_40ms: Some(recall),
            tempo_absolute_error_bpm: Some(if valid { 0.0 } else { 100.0 }),
            primary_tempo_correct: Some(valid),
            grid_phase_error_ms: None,
            grid_phase_correct: None,
            downbeat_status: "unscored".into(),
            meter_status: "unknown".into(),
            core_valid: valid,
        };
        assert_eq!(
            classify_backend_outcome(&snapshot(0.8, true), &snapshot(0.6, true), "native_neural"),
            "native_neural_improved"
        );
        assert_eq!(
            classify_backend_outcome(&snapshot(0.4, false), &snapshot(0.7, true), "native_neural"),
            "native_neural_regressed"
        );
        assert_eq!(
            classify_backend_outcome(
                &snapshot(0.2, false),
                &snapshot(0.3, false),
                "native_neural"
            ),
            "both_failed"
        );
        assert_eq!(
            classify_backend_outcome(
                &snapshot(0.7, true),
                &snapshot(0.7, true),
                "classical_fallback"
            ),
            "fallback_classical_rescued"
        );
        assert_eq!(
            classify_backend_outcome(&snapshot(0.7, true), &snapshot(0.7, true), "native_neural"),
            "mixed_or_equal"
        );
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
            schema_version: OBSERVATION_SCHEMA_VERSION,
            sample_id: first.spec.id.clone(),
            audio_file: None,
            audio_sha256: "b".repeat(64),
            pcm_sha256: None,
            ground_truth_sha256: None,
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
                schema_version: OBSERVATION_SCHEMA_VERSION,
                observations: vec![observation],
            },
            &manifest,
            &[],
            None,
        );
        assert!(result.is_err());
    }

    #[test]
    fn external_identity_checks_wav_pcm_and_ground_truth_independently() {
        let identity = ExternalIdentity {
            wav_file_sha256: "a".repeat(64),
            pcm_sha256: "b".repeat(64),
            ground_truth_sha256: "c".repeat(64),
        };
        let mut observation = observation_with("sample", None, None, None, None, None, false);
        observation.audio_file = Some("audio/sample.wav".into());
        observation.audio_sha256 = identity.wav_file_sha256.clone();
        observation.pcm_sha256 = Some(identity.pcm_sha256.clone());
        observation.ground_truth_sha256 = Some(identity.ground_truth_sha256.clone());
        assert!(verify_external_observation_identity(&observation, &identity).is_ok());
        observation.audio_sha256 = "d".repeat(64);
        assert!(verify_external_observation_identity(&observation, &identity).is_err());
        observation.audio_sha256 = identity.wav_file_sha256.clone();
        observation.pcm_sha256 = Some("d".repeat(64));
        assert!(verify_external_observation_identity(&observation, &identity).is_err());
        observation.pcm_sha256 = Some(identity.pcm_sha256.clone());
        observation.ground_truth_sha256 = Some("d".repeat(64));
        assert!(verify_external_observation_identity(&observation, &identity).is_err());
    }

    #[test]
    fn generated_float_external_identity_is_rejected_by_public_evaluator() {
        let manifest = generate_default_manifest(1).unwrap();
        let observation = observation_with(
            &manifest.fixtures[0].spec.id,
            None,
            None,
            None,
            None,
            None,
            false,
        );
        let result = evaluate_manifest(
            &manifest,
            Some(&ExternalObservationDocument {
                schema_version: OBSERVATION_SCHEMA_VERSION,
                observations: vec![observation],
            }),
            EvaluationOptions {
                mode: AnalyzerMode::Classical,
                split: manifest.split.clone(),
                source_commit: None,
                include_backend_comparison: false,
            },
        );
        assert!(
            matches!(result, Err(LabError::InvalidInput(message)) if message.contains("exported-WAV-only"))
        );
    }

    #[test]
    fn schema_v2_requires_meter_truth_presence_but_accepts_null() {
        let manifest = generate_default_manifest(3).unwrap();
        let mut value = serde_json::to_value(&manifest.fixtures[0].spec).unwrap();
        value.as_object_mut().unwrap().remove("meter_truth");
        assert!(serde_json::from_value::<FixtureSpec>(value).is_err());
        let ambiguous = manifest
            .fixtures
            .iter()
            .find(|fixture| fixture.spec.id == "meter-ambiguous-4-4")
            .unwrap();
        let value = serde_json::to_value(&ambiguous.spec).unwrap();
        let decoded: FixtureSpec = serde_json::from_value(value).unwrap();
        assert_eq!(decoded.meter_truth, None);
    }

    #[test]
    fn platform_participates_in_external_observer_grouping() {
        let mut mac = observation_with("sample", None, None, None, None, None, false);
        mac.audio_file = Some("audio/sample.wav".into());
        let mut windows = mac.clone();
        windows.observer.platform = Some("windows".into());
        mac.observer.platform = Some("macos".into());
        let document = ExternalObservationDocument {
            schema_version: OBSERVATION_SCHEMA_VERSION,
            observations: vec![mac, windows],
        };
        assert!(document.validate().is_ok());
    }

    #[test]
    fn external_report_emits_distinct_platform_groups() {
        let spec = spec(
            "platform-report-sample",
            FixtureFamily::ConstantTempo,
            TempoProfile::Constant { bpm: 120.0 },
            EventStyle::Standard,
            4,
            0,
            101,
        );
        let fixture = generate_fixture(&spec).unwrap();
        let prediction = normalized_prediction(&fixture.truth.beat_times_micros);
        let record = FixtureRecord {
            spec: fixture.spec.clone(),
            truth: fixture.truth.clone(),
            audio_sha256: fixture.audio_sha256.clone(),
        };
        let manifest = SyntheticCorpusManifest {
            schema_version: LAB_SCHEMA_VERSION,
            split: "development".into(),
            seed: 101,
            fixtures: vec![record],
        };
        let track = TrackEvaluation {
            sample_id: fixture.spec.id.clone(),
            family: fixture.spec.family.as_str().into(),
            audio_sha256: fixture.audio_sha256.clone(),
            analysis_backend: "classical".into(),
            neural_diagnostics: None,
            meter_evidence: meter_evidence(&prediction),
            truth: fixture.truth.clone(),
            metrics: metrics_for(&fixture.truth, &prediction),
            wotoha: prediction,
            failure_clusters: Vec::new(),
            human_review: None,
        };
        let mut mac = observation_with(
            &fixture.spec.id,
            Some(fixture.truth.beat_times_micros.clone()),
            Some(fixture.truth.downbeats.clone()),
            Some(120.0),
            Some(4),
            Some(0),
            true,
        );
        mac.audio_file = Some("audio/platform-report-sample.wav".into());
        mac.audio_sha256 = fixture.audio_sha256.clone();
        mac.observer.platform = Some("macOS".into());
        let mut windows = mac.clone();
        windows.observer.platform = Some("Windows".into());
        let report = compare_external(
            &ExternalObservationDocument {
                schema_version: OBSERVATION_SCHEMA_VERSION,
                observations: vec![mac, windows],
            },
            &manifest,
            &[track],
            None,
        )
        .unwrap();
        assert_eq!(report.by_observer.len(), 2);
        assert!(
            report
                .by_observer
                .values()
                .any(|observer| observer.platform == "macOS")
        );
        assert!(
            report
                .by_observer
                .values()
                .any(|observer| observer.platform == "Windows")
        );
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
            audio_file: None,
            audio_sha256: fixture.audio_sha256.clone(),
            pcm_sha256: None,
            ground_truth_sha256: None,
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
        let result = evaluate_manifest(
            &manifest,
            Some(&ExternalObservationDocument {
                schema_version: OBSERVATION_SCHEMA_VERSION,
                observations: vec![observation],
            }),
            EvaluationOptions {
                mode: AnalyzerMode::Classical,
                split: "development".into(),
                source_commit: None,
                include_backend_comparison: false,
            },
        );
        assert!(result.is_err());
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

    fn variable_truth() -> AnalysisGroundTruth {
        AnalysisGroundTruth {
            tempo: None,
            meter: Some(4),
            beat_times_micros: vec![0, 500_000, 995_000, 1_480_000, 1_950_000],
            downbeats: vec![0],
            tempo_segments: vec![TempoSegmentTruth {
                start_micros: 0,
                end_micros: 2_000_000,
                start_bpm: 120.0,
                end_bpm: 126.0,
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
            audio_file: None,
            audio_sha256: "a".repeat(64),
            pcm_sha256: None,
            ground_truth_sha256: None,
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
        assert_eq!(absent_metrics.beat.scored_tracks, 0);
        assert_eq!(absent_metrics.beat.unobserved_tracks, 1);
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
        assert_eq!(empty_metrics.beat.scored_tracks, 1);
        assert_eq!(empty_metrics.beat.unobserved_tracks, 0);
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
    fn external_variable_tempo_phase_is_not_scored() {
        let metrics = external_metrics_for(
            &variable_truth(),
            &normalized_external(&observation_with(
                "sample",
                Some(vec![0, 500_000, 995_000, 1_480_000, 1_950_000]),
                None,
                Some(123.0),
                None,
                Some(0),
                true,
            )),
        );
        assert_eq!(metrics.grid_phase.scored_tracks, 0);
    }

    #[test]
    fn stable_external_grid_can_score_wotoha_phase() {
        let external = normalized_external(&observation_with(
            "sample",
            Some(vec![0, 500_000, 1_000_000, 1_500_000]),
            None,
            None,
            None,
            None,
            true,
        ));
        let metrics = metrics_between_predictions(
            &normalized_prediction(&[0, 500_000, 1_000_000, 1_500_000]),
            &external,
        );
        assert_eq!(metrics.grid_phase.scored_tracks, 1);
        assert_eq!(metrics.grid_phase.correct, Some(true));

        let explicit_phase = normalized_external(&observation_with(
            "sample",
            None,
            None,
            Some(120.0),
            None,
            Some(0),
            true,
        ));
        let explicit_metrics = metrics_between_predictions(
            &normalized_prediction(&[0, 500_000, 1_000_000, 1_500_000]),
            &explicit_phase,
        );
        assert_eq!(explicit_metrics.grid_phase.scored_tracks, 1);
    }

    #[test]
    fn drifting_external_grid_does_not_score_global_phase() {
        let external = normalized_external(&observation_with(
            "sample",
            Some(vec![0, 500_000, 995_000, 1_480_000, 1_950_000]),
            None,
            None,
            None,
            None,
            true,
        ));
        let metrics = metrics_between_predictions(
            &normalized_prediction(&[0, 500_000, 1_000_000, 1_500_000]),
            &external,
        );
        assert_eq!(metrics.grid_phase.scored_tracks, 0);
    }

    #[test]
    fn stable_external_period_uses_median_and_tolerates_one_modest_outlier() {
        let period = stable_external_period(&[0, 500_000, 1_000_000, 1_510_000, 2_010_000]);
        assert_eq!(period, Some(500_000));
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
        let result = evaluate_manifest(
            &manifest,
            Some(&document),
            EvaluationOptions {
                mode: AnalyzerMode::Classical,
                split: "development".into(),
                source_commit: None,
                include_backend_comparison: false,
            },
        );
        assert!(result.is_err());
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
        let metrics = transform_metrics(&specs, &normalized, &BTreeMap::new());
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
        transformed.base_id = Some("base".into());
        transformed.transform = TransformKind::SampleRate {
            sample_rate: 16_000,
        };
        transformed.sample_rate = 16_000;
        let base_spec = FixtureSpec {
            transform: TransformKind::None,
            base_id: None,
            id: "base".into(),
            sample_rate: DEFAULT_SAMPLE_RATE,
            ..transformed.clone()
        };
        let base = generate_fixture(&base_spec).unwrap();
        let converted = generate_fixture(&transformed).unwrap();
        let base_duration = base.audio.len() as f64 / f64::from(base.spec.sample_rate);
        let converted_duration =
            converted.audio.len() as f64 / f64::from(converted.spec.sample_rate);
        let duration_tolerance =
            1.0 / f64::from(base.spec.sample_rate.min(converted.spec.sample_rate));
        assert!((base_duration - converted_duration).abs() <= duration_tolerance);
        assert_eq!(base.spec.sample_rate, 22_050);
        assert_eq!(converted.spec.sample_rate, 16_000);
        assert_eq!(converted.spec.base_id.as_deref(), Some("base"));
        assert_ne!(base.audio_sha256, converted.audio_sha256);
        assert_eq!(
            base.truth.beat_times_micros,
            converted.truth.beat_times_micros
        );
        assert_eq!(base.truth.downbeats, converted.truth.downbeats);
        assert_eq!(base.truth.tempo, converted.truth.tempo);
        assert_eq!(base.truth.meter, converted.truth.meter);
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
            analysis_backend: "test".into(),
            neural_diagnostics: None,
            meter_evidence: MeterEvidenceReport::default(),
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

    fn synthetic_activation(period: f32, phase: f32, length: usize) -> Vec<f32> {
        (0..length)
            .map(|index| {
                let frame = index as f32;
                let nearest = ((frame - phase) / period).round();
                let distance = (frame - (phase + nearest * period)).abs();
                7.0 - 3.0 * distance.min(4.0)
            })
            .collect()
    }

    fn meter_prediction(meter: u8) -> NormalizedAnalysis {
        let beats = (0..48)
            .map(|index| NormalizedBeat {
                time_micros: index as u64 * 500_000,
                timing_confidence: 0.9,
                beat_model_score: Some(0.8),
                onset_support: None,
                low_frequency_support: None,
                downbeat_evidence: Some(if index % usize::from(meter) == 0 {
                    0.95
                } else {
                    0.05
                }),
            })
            .collect();
        NormalizedAnalysis {
            analyzer: "research-test".into(),
            duration_micros: 24_000_000,
            beats,
            tempo_hypotheses: Vec::new(),
            meter_hypotheses: Vec::new(),
            resolved_meter: None,
            resolved_meter_phase: None,
            structure: NormalizedStructure::default(),
            confidence: ConfidenceEvidence::default(),
            provenance: BTreeMap::new(),
        }
    }

    #[test]
    fn fractional_refinement_is_deterministic_and_does_not_change_events() {
        let observations = wotoha_core::beat_analysis::NeuralBeatObservations::with_frame_rate(
            50.0,
            synthetic_activation(24.0, 2.0, 600),
            vec![-7.0; 600],
        )
        .unwrap();
        let first = fractional_tempo_refinement(&observations, Some(125.0), "primary").unwrap();
        let second = fractional_tempo_refinement(&observations, Some(125.0), "primary").unwrap();
        assert_eq!(first, second);
        assert!((first.selected_period_frames.unwrap() - 24.0).abs() < 0.15);
        assert_eq!(first.relation, "primary");
        assert_eq!(observations.beat_logits.len(), 600);
    }

    #[test]
    fn fractional_refinement_recovers_between_frame_period_and_128_case() {
        let between = wotoha_core::beat_analysis::NeuralBeatObservations::with_frame_rate(
            50.0,
            synthetic_activation(24.25, 2.0, 600),
            vec![-7.0; 600],
        )
        .unwrap();
        let refined = fractional_tempo_refinement(&between, Some(125.0), "half_time").unwrap();
        assert!((refined.selected_period_frames.unwrap() - 24.25).abs() < 0.25);
        assert_eq!(refined.relation, "half_time");

        let one_twenty_eight = wotoha_core::beat_analysis::NeuralBeatObservations::with_frame_rate(
            50.0,
            synthetic_activation(50.0 * 60.0 / 128.0, 2.0, 600),
            vec![-7.0; 600],
        )
        .unwrap();
        let refined =
            fractional_tempo_refinement(&one_twenty_eight, Some(125.0), "primary").unwrap();
        assert!((refined.selected_bpm.unwrap() - 128.0).abs() < 1.5);
    }

    #[test]
    fn fractional_refinement_rejects_out_of_range_period() {
        let observations = wotoha_core::beat_analysis::NeuralBeatObservations::with_frame_rate(
            50.0,
            vec![0.0; 100],
            vec![0.0; 100],
        )
        .unwrap();
        assert!(fractional_tempo_refinement(&observations, Some(5_000.0), "primary").is_none());
    }

    #[test]
    fn beat_event_refinement_is_deterministic_and_preserves_relation() {
        let mut beat_logits = vec![-7.0; 700];
        for frame in (25..650).step_by(24) {
            beat_logits[frame] = 7.0;
        }
        let observations = wotoha_core::beat_analysis::NeuralBeatObservations::with_frame_rate(
            50.0,
            beat_logits,
            vec![-7.0; 700],
        )
        .unwrap();
        let first = beat_event_interval_refinement(&observations, "half_time");
        let second = beat_event_interval_refinement(&observations, "half_time");
        assert_eq!(first.selected_bpm, second.selected_bpm);
        assert_eq!(first.relation, "half_time");
        assert_eq!(first.middle_period_micros, second.middle_period_micros);
        assert_eq!(observations.beat_logits.len(), 700);
    }

    #[test]
    fn beat_event_refinement_uses_multi_event_spans_for_fractional_periods() {
        let mut beat_logits = vec![-7.0; 900];
        let mut frame = 25usize;
        for index in 0..30 {
            beat_logits[frame] = 7.0;
            frame = frame.saturating_add(if index % 2 == 0 { 24 } else { 25 });
        }
        let observations = wotoha_core::beat_analysis::NeuralBeatObservations::with_frame_rate(
            50.0,
            beat_logits,
            vec![-7.0; 900],
        )
        .unwrap();
        let refinement = beat_event_interval_refinement(&observations, "primary");
        assert!(refinement.available);
        assert!((refinement.selected_period_micros.unwrap() - 490_000.0).abs() < 20_000.0);
        assert_eq!(refinement.relation, "primary");
    }

    #[test]
    fn beat_event_refinement_rejects_strong_global_drift() {
        let mut beat_logits = vec![-7.0; 1_100];
        let mut frame = 25usize;
        for index in 0..30 {
            beat_logits[frame] = 7.0;
            frame = frame.saturating_add(if index < 15 { 24 } else { 30 });
        }
        let observations = wotoha_core::beat_analysis::NeuralBeatObservations::with_frame_rate(
            50.0,
            beat_logits,
            vec![-7.0; 1_100],
        )
        .unwrap();
        let refinement = beat_event_interval_refinement(&observations, "primary");
        assert!(!refinement.available);
        assert!(refinement.reason.is_some());
    }

    #[test]
    fn component_expanded_family_holdout_excludes_transitive_leakage() {
        let mut rows = vec![
            GateFeatureRow {
                sample_id: "a".into(),
                family: "constant_tempo".into(),
                pcm_group: "pcm-a".into(),
                lineage_group: "lineage-a".into(),
                leakage_group: String::new(),
                neural_available: true,
                features: BTreeMap::new(),
                diagnostic_score: 0.1,
                truth_side_label: "classical_dominates".into(),
            },
            GateFeatureRow {
                sample_id: "b".into(),
                family: "transform".into(),
                pcm_group: "pcm-a".into(),
                lineage_group: "lineage-b".into(),
                leakage_group: String::new(),
                neural_available: true,
                features: BTreeMap::new(),
                diagnostic_score: 0.2,
                truth_side_label: "equal".into(),
            },
            GateFeatureRow {
                sample_id: "c".into(),
                family: "percussion".into(),
                pcm_group: "pcm-c".into(),
                lineage_group: "lineage-b".into(),
                leakage_group: String::new(),
                neural_available: true,
                features: BTreeMap::new(),
                diagnostic_score: 0.3,
                truth_side_label: "equal".into(),
            },
            GateFeatureRow {
                sample_id: "d".into(),
                family: "meter".into(),
                pcm_group: "pcm-d".into(),
                lineage_group: "lineage-d".into(),
                leakage_group: String::new(),
                neural_available: true,
                features: BTreeMap::new(),
                diagnostic_score: 0.4,
                truth_side_label: "equal".into(),
            },
        ];
        let groups = connected_leakage_groups(&rows);
        for row in &mut rows {
            row.leakage_group = groups[&row.sample_id].clone();
        }
        let folds = component_expanded_leave_family_out_folds(&rows);
        let fold = folds
            .iter()
            .find(|fold| fold.validation_groups == vec!["constant_tempo"])
            .unwrap();
        assert_eq!(fold.validation_split, vec!["a", "b", "c"]);
        assert!(!fold.exact_pcm_overlap);
        assert!(!fold.lineage_overlap);
    }

    #[test]
    fn contrastive_meter_phase_resolves_clear_meters_and_allows_unknown() {
        for meter in [2_u8, 3, 4, 6] {
            let prediction = meter_prediction(meter);
            let (resolved, candidates, margin, reason) = experimental_meter_resolution(&prediction);
            assert_eq!(resolved, Some(meter));
            assert_eq!(candidates.len(), 15);
            assert!(margin.is_some());
            assert!(reason.is_none());
        }
        let mut ambiguous = meter_prediction(4);
        for beat in &mut ambiguous.beats {
            beat.downbeat_evidence = Some(0.5);
        }
        let (resolved, _, margin, reason) = experimental_meter_resolution(&ambiguous);
        assert!(resolved.is_none());
        assert!(margin.is_some_and(|value| value < 0.04));
        assert!(reason.is_some());
    }

    #[test]
    fn meter_off_phase_leakage_lowers_contrastive_score_and_production_is_unchanged() {
        let prediction = meter_prediction(4);
        let clean = meter_phase_candidate(&prediction, 4, 0);
        let mut leaky = prediction.clone();
        for (index, beat) in leaky.beats.iter_mut().enumerate() {
            if index % 4 == 1 {
                beat.downbeat_evidence = Some(0.8);
            }
        }
        let leaky = meter_phase_candidate(&leaky, 4, 0);
        assert!(leaky.off_phase_downbeat_leakage > clean.off_phase_downbeat_leakage);
        assert!(leaky.score < clean.score);
        assert_eq!(prediction.resolved_meter, None);
    }

    #[test]
    fn gate_features_are_predecision_only_and_grouped_duplicates_do_not_split() {
        let rows = vec![
            GateFeatureRow {
                sample_id: "a".into(),
                family: "constant_tempo".into(),
                pcm_group: "same".into(),
                lineage_group: "root".into(),
                leakage_group: "a".into(),
                neural_available: true,
                features: BTreeMap::from([("candidate_score".into(), 0.8)]),
                diagnostic_score: 0.8,
                truth_side_label: "neural_dominates".into(),
            },
            GateFeatureRow {
                sample_id: "b".into(),
                family: "transform".into(),
                pcm_group: "same".into(),
                lineage_group: "root".into(),
                leakage_group: "a".into(),
                neural_available: true,
                features: BTreeMap::from([("candidate_score".into(), 0.2)]),
                diagnostic_score: 0.2,
                truth_side_label: "classical_dominates".into(),
            },
        ];
        let fold = gate_validation_fold(&rows, &[0, 1], "exact_pcm");
        assert!(!fold.exact_pcm_overlap);
        assert!(!GATE_FEATURE_NAMES.iter().any(|name| {
            ["fixture_id", "family", "ground_truth", "expected_bpm"].contains(name)
        }));
        assert!(!rows[0].features.contains_key("fixture_id"));
    }

    #[test]
    fn joint_leakage_groups_are_transitive_stable_and_disjoint_in_primary_folds() {
        let mut rows = vec![
            GateFeatureRow {
                sample_id: "a".into(),
                family: "one".into(),
                pcm_group: "pcm-a".into(),
                lineage_group: "lineage-a".into(),
                leakage_group: String::new(),
                neural_available: true,
                features: BTreeMap::new(),
                diagnostic_score: 0.2,
                truth_side_label: "classical_dominates".into(),
            },
            GateFeatureRow {
                sample_id: "b".into(),
                family: "two".into(),
                pcm_group: "pcm-b".into(),
                lineage_group: "lineage-a".into(),
                leakage_group: String::new(),
                neural_available: true,
                features: BTreeMap::new(),
                diagnostic_score: 0.3,
                truth_side_label: "mixed".into(),
            },
            GateFeatureRow {
                sample_id: "c".into(),
                family: "three".into(),
                pcm_group: "pcm-b".into(),
                lineage_group: "lineage-c".into(),
                leakage_group: String::new(),
                neural_available: true,
                features: BTreeMap::new(),
                diagnostic_score: 0.4,
                truth_side_label: "equal".into(),
            },
            GateFeatureRow {
                sample_id: "d".into(),
                family: "four".into(),
                pcm_group: "pcm-d".into(),
                lineage_group: "lineage-d".into(),
                leakage_group: String::new(),
                neural_available: false,
                features: BTreeMap::new(),
                diagnostic_score: 0.0,
                truth_side_label: "classical_dominates".into(),
            },
        ];
        rows.sort_by(|left, right| left.sample_id.cmp(&right.sample_id));
        let first = connected_leakage_groups(&rows);
        let second = connected_leakage_groups(&rows);
        assert_eq!(first, second);
        assert_eq!(first["a"], "a");
        assert_eq!(first["b"], "a");
        assert_eq!(first["c"], "a");
        assert_eq!(first["d"], "d");
        for row in &mut rows {
            row.leakage_group = first[&row.sample_id].clone();
        }
        let folds = grouped_validation_folds(&rows, "joint_leakage");
        assert_eq!(folds.len(), 2);
        for fold in &folds {
            assert!(!fold.exact_pcm_overlap);
            assert!(!fold.lineage_overlap);
            assert!(fold.overlapping_pcm_hashes.is_empty());
            assert!(fold.overlapping_lineage_groups.is_empty());
        }
        let decisions = folds
            .iter()
            .flat_map(|fold| fold.validation_split.iter().cloned())
            .collect::<Vec<_>>();
        assert_eq!(decisions.len(), rows.len());
        assert_eq!(decisions.iter().collect::<BTreeSet<_>>().len(), rows.len());
    }

    #[test]
    fn gate_training_degenerate_folds_choose_classical_deterministically() {
        let (first, first_reason) = fit_gate_threshold_with_reason(&[]);
        let (second, second_reason) = fit_gate_threshold_with_reason(&[]);
        assert_eq!(first, second);
        assert_eq!(first_reason, second_reason);
        assert!(first > 1.0);
        assert!(first_reason.contains("Always Classical"));
    }

    #[test]
    fn direct_overlap_audit_reports_pcm_and_lineage_separately() {
        let rows = vec![
            GateFeatureRow {
                sample_id: "a".into(),
                family: "a".into(),
                pcm_group: "same-pcm".into(),
                lineage_group: "lineage-a".into(),
                leakage_group: "a".into(),
                neural_available: true,
                features: BTreeMap::new(),
                diagnostic_score: 0.5,
                truth_side_label: "classical_dominates".into(),
            },
            GateFeatureRow {
                sample_id: "b".into(),
                family: "b".into(),
                pcm_group: "same-pcm".into(),
                lineage_group: "lineage-b".into(),
                leakage_group: "b".into(),
                neural_available: true,
                features: BTreeMap::new(),
                diagnostic_score: 0.5,
                truth_side_label: "classical_dominates".into(),
            },
        ];
        let fold = gate_validation_fold(&rows, &[0], "sample");
        assert!(fold.exact_pcm_overlap);
        assert!(!fold.lineage_overlap);
        assert_eq!(fold.overlapping_pcm_hashes, vec!["same-pcm"]);
        assert!(fold.overlapping_lineage_groups.is_empty());
    }

    #[test]
    fn dimension_oracles_choose_independently_and_joint_labels_preserve_mixed() {
        let mut neural = BackendMetricSnapshot {
            beat_mae_ms: Some(1.0),
            beat_p50_ms: Some(1.0),
            beat_p95_ms: Some(2.0),
            precision_at_40ms: Some(0.9),
            recall_at_40ms: Some(0.9),
            tempo_absolute_error_bpm: Some(4.0),
            primary_tempo_correct: Some(false),
            grid_phase_error_ms: Some(20.0),
            grid_phase_correct: Some(false),
            downbeat_status: "unscored".into(),
            meter_status: "unknown".into(),
            core_valid: true,
        };
        let classical = BackendMetricSnapshot {
            beat_mae_ms: Some(10.0),
            beat_p50_ms: Some(10.0),
            beat_p95_ms: Some(20.0),
            precision_at_40ms: Some(0.5),
            recall_at_40ms: Some(0.5),
            tempo_absolute_error_bpm: Some(1.0),
            primary_tempo_correct: Some(true),
            grid_phase_error_ms: Some(2.0),
            grid_phase_correct: Some(true),
            downbeat_status: "unscored".into(),
            meter_status: "unknown".into(),
            core_valid: true,
        };
        assert_eq!(
            choose_backend_for_lower(neural.beat_mae_ms, classical.beat_mae_ms),
            "neural"
        );
        assert_eq!(
            choose_backend_for_tempo(true, &neural, &classical),
            "classical"
        );
        assert_eq!(
            choose_backend_for_grid_phase(true, &neural, &classical),
            "classical"
        );
        let thresholds = BackendMaterialThresholds {
            beat_mae_ms: 1.0,
            beat_p95_ms: 5.0,
            precision_recall: 0.02,
        };
        assert_eq!(truth_dominance(&neural, &classical, &thresholds), "mixed");
        neural.primary_tempo_correct = Some(true);
        neural.grid_phase_correct = Some(true);
        assert_eq!(
            truth_dominance(&neural, &classical, &thresholds),
            "neural_dominates"
        );
    }

    #[test]
    fn gate_features_distinguish_best_and_selected_candidates() {
        let truth = truth_with_meter(Some(4));
        let prediction = normalized_prediction(&truth.beat_times_micros);
        let diagnostics = NeuralDiagnostics {
            selected_period_frames: Some(20),
            selected_bpm: Some(150.0),
            path_marker_count: 1,
            path_coverage: Some(0.5),
            activation_mean: Some(0.5),
            support: Some(0.5),
            interval_residual: Some(0.1),
            alias_margin: Some(0.2),
            candidates: vec![
                NeuralCandidateDiagnostics {
                    bpm: 120.0,
                    relation: "primary".into(),
                    period_frames: 25,
                    available: true,
                    normalized_weight: Some(0.6),
                    best_phase_frames: Some(0),
                    activation_evidence: 0.9,
                    coverage: 0.9,
                    off_grid_leakage: 0.8,
                    periodic_consistency: 0.9,
                    candidate_score: 0.9,
                },
                NeuralCandidateDiagnostics {
                    bpm: 150.0,
                    relation: "primary".into(),
                    period_frames: 20,
                    available: true,
                    normalized_weight: Some(0.4),
                    best_phase_frames: Some(0),
                    activation_evidence: 0.4,
                    coverage: 0.4,
                    off_grid_leakage: 0.1,
                    periodic_consistency: 0.2,
                    candidate_score: 0.4,
                },
            ],
            decoder_accepted: true,
            rejection_reason: None,
        };
        let track = TrackEvaluation {
            sample_id: "feature-test".into(),
            family: "constant_tempo".into(),
            audio_sha256: "a".repeat(64),
            analysis_backend: "native_neural".into(),
            neural_diagnostics: Some(diagnostics),
            meter_evidence: meter_evidence(&prediction),
            truth,
            metrics: GroupMetrics::default(),
            wotoha: prediction,
            failure_clusters: Vec::new(),
            human_review: None,
        };
        let (features, _, _) = gate_features(&track);
        assert!((features["best_candidate_score"] - 0.9).abs() < 1e-6);
        assert!((features["selected_candidate_score"] - 0.4).abs() < 1e-6);
        assert!((features["selected_candidate_off_grid_leakage"] - 0.1).abs() < 1e-6);
        assert!((features["selected_candidate_periodic_consistency"] - 0.2).abs() < 1e-6);
        assert_eq!(features["best_vs_selected_relation_disagreement"], 0.0);
        assert!(!features.contains_key("sample_id"));
        assert!(!features.contains_key("family"));
        assert!(!features.contains_key("expected_bpm"));
        assert!(!features.contains_key("ground_truth"));
    }

    #[test]
    fn tempo_refinement_primary_cohort_excludes_fallback_and_separates_relations() {
        let observations = wotoha_core::beat_analysis::NeuralBeatObservations::with_frame_rate(
            50.0,
            synthetic_activation(24.0, 2.0, 300),
            vec![-7.0; 300],
        )
        .unwrap();
        let fixture = ExperimentalTempoFixture {
            sample_id: "native-tempo".into(),
            family: "constant_tempo".into(),
            truth_bpm: Some(128.0),
            current_primary_bpm: Some(125.0),
            current_relation_to_truth: "other_wrong".into(),
            pcm_relation_to_truth: "other_wrong".into(),
            activation_relation_to_truth: Some("other_wrong".into()),
            pcm: ExperimentalTempoResolution::default(),
            activation: None,
            fractional_refinement: None,
            raw_observations: Some(observations),
        };
        let truth = truth_with_meter(Some(4));
        let prediction = normalized_prediction(&truth.beat_times_micros);
        let diagnostics = NeuralDiagnostics {
            selected_period_frames: Some(24),
            selected_bpm: Some(125.0),
            path_marker_count: 1,
            path_coverage: Some(0.8),
            activation_mean: Some(0.8),
            support: Some(0.8),
            interval_residual: Some(0.1),
            alias_margin: Some(0.2),
            candidates: vec![NeuralCandidateDiagnostics {
                bpm: 125.0,
                relation: "primary".into(),
                period_frames: 24,
                available: true,
                normalized_weight: Some(1.0),
                best_phase_frames: Some(0),
                activation_evidence: 0.8,
                coverage: 0.8,
                off_grid_leakage: 0.1,
                periodic_consistency: 0.9,
                candidate_score: 0.8,
            }],
            decoder_accepted: true,
            rejection_reason: None,
        };
        let native = TrackEvaluation {
            sample_id: "native-tempo".into(),
            family: "constant_tempo".into(),
            audio_sha256: "a".repeat(64),
            analysis_backend: "native_neural".into(),
            neural_diagnostics: Some(diagnostics),
            meter_evidence: meter_evidence(&prediction),
            truth,
            metrics: GroupMetrics::default(),
            wotoha: prediction,
            failure_clusters: Vec::new(),
            human_review: None,
        };
        assert_eq!(
            tempo_refinement_primary_cohort(&fixture, &native),
            (true, None)
        );
        assert_eq!(production_selection_relation(&native), "primary");
        assert_eq!(relation_to_truth(Some(125.0), 128.0, false), "other_wrong");
        assert_eq!(relation_to_truth(Some(128.0), 128.0, false), "primary");

        let mut fallback = native;
        fallback.analysis_backend = "classical_fallback".into();
        assert_eq!(
            tempo_refinement_primary_cohort(&fixture, &fallback),
            (false, Some("classical_fallback".into()))
        );
    }

    #[test]
    fn meter_research_scope_is_family_and_truth_semantics_not_fixture_id() {
        assert!(!is_meter_research_family("constant_tempo"));
        assert!(!is_meter_research_family("transform"));
        assert!(is_meter_research_family(FixtureFamily::Meter.as_str()));
        for truth in [Some(2), Some(3), Some(4), Some(6), None] {
            assert_eq!(meter_research_truth_is_clear(truth), truth.is_some());
        }
    }
}
