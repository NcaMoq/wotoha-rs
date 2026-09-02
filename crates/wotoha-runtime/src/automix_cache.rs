use std::{
    fs::{self, File, OpenOptions},
    io::{BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use wotoha_core::{
    PreparedSource, TrackRequest,
    automix::{KeyMode, MusicalKey, TrackAnalysis},
};

// Analysis schema V2 is intentionally kept out of `wotoha-core`'s cache
// implementation.  The domain owns the values and their validation; this
// module owns the on-disk envelope, version, and codec.
use wotoha_core::analysis::structure::SectionLabelScore;
use wotoha_core::analysis::{
    AnalysisMethod, AnalysisProvenance, ComponentProvenance, CueProvenance, DjCue, EnergyAnalysis,
    HumanCueSource, KeyMode as V2KeyMode, LocalTonalWindow, MeterHypothesis, ModelIdentity,
    MusicalKey as V2MusicalKey, PhraseBoundary, PhraseBoundarySource, Relation, RhythmAnalysis,
    Section, SectionLabel, StructureAnalysis, TonalAnalysis, TrackAnalysisV2, VocalAnalysis,
    value::{Confidence, ModelScore, Support, UnitInterval},
};

/// Current analysis cache envelope version. The V2 rollout bumps the former
/// V1 schema (11) to 12; old records are cache misses and are recomputable.
pub const ANALYSIS_CACHE_SCHEMA_VERSION: u32 = 12;
/// V2 records use the current schema in a separate compact namespace. A
/// schema mismatch is a miss; no old file is migrated or rewritten in place.
pub const ANALYSIS_CACHE_V2_SCHEMA_VERSION: u32 = ANALYSIS_CACHE_SCHEMA_VERSION;
/// Maximum encoded size of one V2 cache record.  This is a hard bound checked
/// before decode and before an atomic install.
pub const ANALYSIS_CACHE_V2_MAX_FILE_BYTES: u64 = 1024 * 1024;
/// Sentinel used by compact BeatEvent score arrays. Values 0..=254 are
/// quantized finite scores; 255 means an absent optional score/support.
pub const ANALYSIS_CACHE_V2_UNKNOWN_SCORE: u8 = 255;
pub(crate) const ANALYSIS_CACHE_ANALYZER_VERSION: &str = "pcm-onset-chroma-level-loudness-neural-beat-this-1.0.0-rten-0.24.0-small-a5f8d39d989f31859454ba27afe61c5317ca95e4d9373e6853e5361b8937172f-mel-fdd59e65c515331308e4c8841edf99972deca646bdf6197744c2a5b7755e3de9-v12";
pub(crate) const ANALYSIS_CACHE_CLASSICAL_ANALYZER_VERSION: &str = "pcm-onset-chroma-level-loudness-classical-permanent-neural-eligibility-beat-this-1.0.0-rten-0.24.0-a5f8d39d989f31859454ba27afe61c5317ca95e4d9373e6853e5361b8937172f-fdd59e65c515331308e4c8841edf99972deca646bdf6197744c2a5b7755e3de9-v12";
const MAX_CACHE_FILE_BYTES: u64 = 256 * 1024;
const SOURCE_DURATION_TOLERANCE_MICROS: u64 = 1_000_000;

static NEXT_TEMP_FILE_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct AnalysisCacheKey {
    provider_id: String,
    canonical_key: String,
    content_length: Option<u64>,
    expected_duration_micros: Option<u64>,
}

impl AnalysisCacheKey {
    pub fn new(
        provider_id: impl Into<String>,
        canonical_key: impl Into<String>,
        content_length: Option<u64>,
        expected_duration: Option<Duration>,
    ) -> Result<Self, AnalysisCacheError> {
        let provider_id = provider_id.into();
        let canonical_key = canonical_key.into();
        if provider_id.is_empty() || canonical_key.is_empty() {
            return Err(AnalysisCacheError::InvalidKey);
        }

        Ok(Self {
            provider_id,
            canonical_key,
            content_length,
            expected_duration_micros: expected_duration.map(duration_to_micros),
        })
    }

    pub fn from_request(request: &TrackRequest) -> Result<Self, AnalysisCacheError> {
        let content_length = match &request.prepared {
            PreparedSource::Http { content_length, .. } => *content_length,
            PreparedSource::Hls { .. } => None,
        };
        Self::new(
            request.provider_id.as_ref(),
            request.canonical_key.as_ref(),
            content_length,
            request.metadata.duration,
        )
    }

    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }

    pub fn canonical_key(&self) -> &str {
        &self.canonical_key
    }

    fn digest(&self) -> String {
        let mut digest = Sha256::new();
        update_length_prefixed(&mut digest, self.provider_id.as_bytes());
        update_length_prefixed(&mut digest, self.canonical_key.as_bytes());
        let bytes = digest.finalize();
        let mut encoded = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            use std::fmt::Write as _;
            write!(encoded, "{byte:02x}").expect("writing into a String cannot fail");
        }
        encoded
    }
}

#[derive(Clone, Debug)]
pub struct AnalysisCache {
    root: PathBuf,
    analyzer_version: String,
}

impl AnalysisCache {
    pub fn new(
        root: impl Into<PathBuf>,
        analyzer_version: impl Into<String>,
    ) -> Result<Self, AnalysisCacheError> {
        let analyzer_version = analyzer_version.into();
        if analyzer_version.is_empty() {
            return Err(AnalysisCacheError::InvalidAnalyzerVersion);
        }
        Ok(Self {
            root: root.into(),
            analyzer_version,
        })
    }

    pub fn load(
        &self,
        key: &AnalysisCacheKey,
    ) -> Result<Option<TrackAnalysis>, AnalysisCacheError> {
        let path = self.path_for(key);
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(io_error("open", path, source)),
        };
        let size = file
            .metadata()
            .map_err(|source| io_error("inspect", path.clone(), source))?
            .len();
        if size > MAX_CACHE_FILE_BYTES {
            return Err(AnalysisCacheError::Oversized { path, size });
        }

        let record: CachedAnalysis =
            serde_json::from_reader(BufReader::new(file)).map_err(|source| {
                AnalysisCacheError::Decode {
                    path: path.clone(),
                    source,
                }
            })?;
        if !record.matches(key, &self.analyzer_version) {
            return Ok(None);
        }
        record.analysis.try_into().map(Some)
    }

    pub fn store(
        &self,
        key: &AnalysisCacheKey,
        analysis: &TrackAnalysis,
    ) -> Result<(), AnalysisCacheError> {
        validate_analysis(analysis)?;
        fs::create_dir_all(&self.root)
            .map_err(|source| io_error("create cache directory", self.root.clone(), source))?;

        let path = self.path_for(key);
        let temp_path = self.temp_path_for(key);
        let record = CachedAnalysis::new(key, &self.analyzer_version, analysis);
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp_path)
            .map_err(|source| io_error("create temporary cache file", temp_path.clone(), source))?;

        let write_result = (|| {
            let mut writer = BufWriter::new(file);
            serde_json::to_writer(&mut writer, &record).map_err(AnalysisCacheError::Encode)?;
            writer.flush().map_err(|source| {
                io_error("flush temporary cache file", temp_path.clone(), source)
            })?;
            writer.get_ref().sync_all().map_err(|source| {
                io_error("sync temporary cache file", temp_path.clone(), source)
            })?;
            drop(writer);
            replace_file(&temp_path, &path)
        })();

        if write_result.is_err() {
            let _ = fs::remove_file(&temp_path);
        }
        write_result
    }

    /// Loads an analysis-schema V2 record from its separate binary cache
    /// namespace.  V1 JSON records are never considered by this method.
    pub fn load_v2(
        &self,
        key: &AnalysisCacheKey,
    ) -> Result<Option<TrackAnalysisV2>, AnalysisCacheError> {
        let path = self.path_for_v2(key);
        let mut file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(io_error("open", path, source)),
        };
        let size = file
            .metadata()
            .map_err(|source| io_error("inspect", path.clone(), source))?
            .len();
        if size > ANALYSIS_CACHE_V2_MAX_FILE_BYTES {
            return Err(AnalysisCacheError::Oversized { path, size });
        }
        let mut bytes = Vec::with_capacity(size as usize);
        file.read_to_end(&mut bytes)
            .map_err(|source| io_error("read", path.clone(), source))?;
        if bytes.len() as u64 > ANALYSIS_CACHE_V2_MAX_FILE_BYTES {
            return Err(AnalysisCacheError::Oversized {
                path,
                size: bytes.len() as u64,
            });
        }
        // Read the envelope version before interpreting the payload. Older
        // V2 layouts are deliberately not migrated; they are cache misses,
        // even if their payload no longer matches the current field shape.
        if bytes.len() >= V2_MAGIC.len() + std::mem::size_of::<u32>()
            && bytes[..V2_MAGIC.len()] == V2_MAGIC
        {
            let schema_offset = V2_MAGIC.len();
            let schema_end = schema_offset + std::mem::size_of::<u32>();
            // The length guard above proves that all four bytes are present.
            let schema_version = u32::from_le_bytes([
                bytes[schema_offset],
                bytes[schema_offset + 1],
                bytes[schema_offset + 2],
                bytes[schema_end - 1],
            ]);
            if schema_version != ANALYSIS_CACHE_V2_SCHEMA_VERSION {
                return Ok(None);
            }
        }
        let record = decode_v2_record(&bytes).map_err(|message| AnalysisCacheError::DecodeV2 {
            path: path.clone(),
            message,
        })?;
        if !record.matches(key, &self.analyzer_version) {
            return Ok(None);
        }
        validate_analysis_v2(&record.analysis)?;
        Ok(Some(record.analysis))
    }

    /// Stores an analysis-schema V2 record using a bounded compact binary
    /// codec and the same fsync + atomic-rename protocol as the V1 cache.
    pub fn store_v2(
        &self,
        key: &AnalysisCacheKey,
        analysis: &TrackAnalysisV2,
    ) -> Result<(), AnalysisCacheError> {
        validate_analysis_v2(analysis)?;
        let record = CachedAnalysisV2::new(key, &self.analyzer_version, analysis);
        let bytes = encode_v2_record(&record).map_err(AnalysisCacheError::EncodeV2)?;
        let size = bytes.len() as u64;
        if size > ANALYSIS_CACHE_V2_MAX_FILE_BYTES {
            return Err(AnalysisCacheError::Oversized {
                path: self.path_for_v2(key),
                size,
            });
        }
        fs::create_dir_all(&self.root)
            .map_err(|source| io_error("create cache directory", self.root.clone(), source))?;

        let path = self.path_for_v2(key);
        let temp_path = self.temp_path_for_v2(key);
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp_path)
            .map_err(|source| io_error("create temporary cache file", temp_path.clone(), source))?;

        let write_result = (|| {
            let mut writer = BufWriter::new(file);
            writer.write_all(&bytes).map_err(|source| {
                io_error("write temporary cache file", temp_path.clone(), source)
            })?;
            writer.flush().map_err(|source| {
                io_error("flush temporary cache file", temp_path.clone(), source)
            })?;
            writer.get_ref().sync_all().map_err(|source| {
                io_error("sync temporary cache file", temp_path.clone(), source)
            })?;
            drop(writer);
            replace_file(&temp_path, &path)
        })();

        if write_result.is_err() {
            let _ = fs::remove_file(&temp_path);
        }
        write_result
    }

    fn path_for(&self, key: &AnalysisCacheKey) -> PathBuf {
        self.root.join(format!("{}.json", key.digest()))
    }

    fn path_for_v2(&self, key: &AnalysisCacheKey) -> PathBuf {
        self.root.join(format!("{}.v2.bin", key.digest()))
    }

    fn temp_path_for(&self, key: &AnalysisCacheKey) -> PathBuf {
        let sequence = NEXT_TEMP_FILE_ID.fetch_add(1, Ordering::Relaxed);
        self.root.join(format!(
            ".{}.{}.{}.tmp",
            key.digest(),
            std::process::id(),
            sequence
        ))
    }

    fn temp_path_for_v2(&self, key: &AnalysisCacheKey) -> PathBuf {
        let sequence = NEXT_TEMP_FILE_ID.fetch_add(1, Ordering::Relaxed);
        self.root.join(format!(
            ".{}.{}.{}.v2.tmp",
            key.digest(),
            std::process::id(),
            sequence
        ))
    }
}

#[derive(Debug, Error)]
pub enum AnalysisCacheError {
    #[error("analysis cache provider and canonical key must not be empty")]
    InvalidKey,
    #[error("analysis cache analyzer version must not be empty")]
    InvalidAnalyzerVersion,
    #[error("invalid track analysis: {0}")]
    InvalidAnalysis(&'static str),
    #[error("failed to {operation} at {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("analysis cache file is too large ({size} bytes): {path}")]
    Oversized { path: PathBuf, size: u64 },
    #[error("failed to decode analysis cache file {path}: {source}")]
    Decode {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to decode analysis cache file {path}: {message}")]
    DecodeV2 { path: PathBuf, message: String },
    #[error("failed to encode analysis cache record: {0}")]
    Encode(#[source] serde_json::Error),
    #[error("failed to encode analysis cache V2 record: {0}")]
    EncodeV2(String),
}

#[derive(Debug, Serialize, Deserialize)]
struct CachedAnalysis {
    schema_version: u32,
    analyzer_version: String,
    provider_id: String,
    canonical_key: String,
    content_length: Option<u64>,
    expected_duration_micros: Option<u64>,
    analysis: SerializableAnalysis,
}

impl CachedAnalysis {
    fn new(key: &AnalysisCacheKey, analyzer_version: &str, analysis: &TrackAnalysis) -> Self {
        Self {
            schema_version: ANALYSIS_CACHE_SCHEMA_VERSION,
            analyzer_version: analyzer_version.to_owned(),
            provider_id: key.provider_id.clone(),
            canonical_key: key.canonical_key.clone(),
            content_length: key.content_length,
            expected_duration_micros: key.expected_duration_micros,
            analysis: SerializableAnalysis::from(analysis),
        }
    }

    fn matches(&self, key: &AnalysisCacheKey, analyzer_version: &str) -> bool {
        self.schema_version == ANALYSIS_CACHE_SCHEMA_VERSION
            && self.analyzer_version == analyzer_version
            && self.provider_id == key.provider_id
            && self.canonical_key == key.canonical_key
            && optional_identity_matches(self.content_length, key.content_length, 0)
            && optional_identity_matches(
                self.expected_duration_micros,
                key.expected_duration_micros,
                SOURCE_DURATION_TOLERANCE_MICROS,
            )
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct SerializableAnalysis {
    duration_micros: u64,
    audible_start_micros: u64,
    audible_end_micros: u64,
    #[serde(default)]
    intro_end_micros: Option<u64>,
    #[serde(default)]
    intro_confidence: f32,
    #[serde(default)]
    outro_start_micros: Option<u64>,
    #[serde(default)]
    outro_confidence: f32,
    #[serde(default)]
    vocal_activity: Vec<u8>,
    #[serde(default)]
    vocal_activity_confidences: Vec<u8>,
    #[serde(default)]
    vocal_activity_rate: u8,
    #[serde(default)]
    energy_profile: Vec<u8>,
    #[serde(default)]
    energy_profile_rate: u8,
    bpm: Option<f32>,
    beat_confidence: f32,
    first_beat_micros: Option<u64>,
    #[serde(default)]
    beat_markers_micros: Vec<u64>,
    #[serde(default)]
    beat_marker_confidences: Vec<u8>,
    first_downbeat_micros: Option<u64>,
    downbeat_confidence: f32,
    musical_key: Option<SerializableMusicalKey>,
    rms_dbfs: Option<f32>,
    sample_peak_dbfs: Option<f32>,
    #[serde(default)]
    integrated_lufs: Option<f32>,
    #[serde(default)]
    true_peak_dbtp: Option<f32>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SerializableMusicalKey {
    tonic: u8,
    minor: bool,
    confidence: f32,
}

impl From<&TrackAnalysis> for SerializableAnalysis {
    fn from(value: &TrackAnalysis) -> Self {
        Self {
            duration_micros: duration_to_micros(value.duration),
            audible_start_micros: duration_to_micros(value.audible_start),
            audible_end_micros: duration_to_micros(value.audible_end),
            intro_end_micros: value.intro_end.map(duration_to_micros),
            intro_confidence: value.intro_confidence,
            outro_start_micros: value.outro_start.map(duration_to_micros),
            outro_confidence: value.outro_confidence,
            vocal_activity: value.vocal_activity.clone(),
            vocal_activity_confidences: value.vocal_activity_confidences.clone(),
            vocal_activity_rate: value.vocal_activity_rate,
            energy_profile: value.energy_profile.clone(),
            energy_profile_rate: value.energy_profile_rate,
            bpm: value.bpm,
            beat_confidence: value.beat_confidence,
            first_beat_micros: value.first_beat.map(duration_to_micros),
            beat_markers_micros: value
                .beat_markers
                .iter()
                .copied()
                .map(duration_to_micros)
                .collect(),
            beat_marker_confidences: value
                .beat_marker_confidences
                .iter()
                .map(|confidence| (confidence.clamp(0.0, 1.0) * 255.0).round() as u8)
                .collect(),
            first_downbeat_micros: value.first_downbeat.map(duration_to_micros),
            downbeat_confidence: value.downbeat_confidence,
            musical_key: value.musical_key.map(|key| SerializableMusicalKey {
                tonic: key.tonic,
                minor: key.mode == KeyMode::Minor,
                confidence: key.confidence,
            }),
            rms_dbfs: value.rms_dbfs,
            sample_peak_dbfs: value.sample_peak_dbfs,
            integrated_lufs: value.integrated_lufs,
            true_peak_dbtp: value.true_peak_dbtp,
        }
    }
}

impl TryFrom<SerializableAnalysis> for TrackAnalysis {
    type Error = AnalysisCacheError;

    fn try_from(value: SerializableAnalysis) -> Result<Self, Self::Error> {
        let analysis = Self {
            duration: Duration::from_micros(value.duration_micros),
            audible_start: Duration::from_micros(value.audible_start_micros),
            audible_end: Duration::from_micros(value.audible_end_micros),
            intro_end: value.intro_end_micros.map(Duration::from_micros),
            intro_confidence: value.intro_confidence,
            outro_start: value.outro_start_micros.map(Duration::from_micros),
            outro_confidence: value.outro_confidence,
            vocal_activity: value.vocal_activity,
            vocal_activity_confidences: value.vocal_activity_confidences,
            vocal_activity_rate: value.vocal_activity_rate,
            energy_profile: value.energy_profile,
            energy_profile_rate: value.energy_profile_rate,
            bpm: value.bpm,
            beat_confidence: value.beat_confidence,
            first_beat: value.first_beat_micros.map(Duration::from_micros),
            beat_markers: value
                .beat_markers_micros
                .into_iter()
                .map(Duration::from_micros)
                .collect(),
            beat_marker_confidences: value
                .beat_marker_confidences
                .into_iter()
                .map(|confidence| f32::from(confidence) / 255.0)
                .collect(),
            first_downbeat: value.first_downbeat_micros.map(Duration::from_micros),
            downbeat_confidence: value.downbeat_confidence,
            musical_key: value.musical_key.map(|key| MusicalKey {
                tonic: key.tonic,
                mode: if key.minor {
                    KeyMode::Minor
                } else {
                    KeyMode::Major
                },
                confidence: key.confidence,
            }),
            rms_dbfs: value.rms_dbfs,
            sample_peak_dbfs: value.sample_peak_dbfs,
            integrated_lufs: value.integrated_lufs,
            true_peak_dbtp: value.true_peak_dbtp,
        };
        validate_analysis(&analysis)?;
        Ok(analysis)
    }
}

fn validate_analysis(analysis: &TrackAnalysis) -> Result<(), AnalysisCacheError> {
    if analysis.duration.is_zero() {
        return Err(AnalysisCacheError::InvalidAnalysis(
            "duration must be greater than zero",
        ));
    }
    if analysis.audible_start > analysis.audible_end || analysis.audible_end > analysis.duration {
        return Err(AnalysisCacheError::InvalidAnalysis(
            "audible boundaries must be ordered within the track duration",
        ));
    }
    if analysis.intro_end.is_some_and(|boundary| {
        boundary < analysis.audible_start || boundary > analysis.audible_end
    }) || analysis.outro_start.is_some_and(|boundary| {
        boundary < analysis.audible_start || boundary > analysis.audible_end
    }) || analysis
        .intro_end
        .zip(analysis.outro_start)
        .is_some_and(|(intro, outro)| intro >= outro)
    {
        return Err(AnalysisCacheError::InvalidAnalysis(
            "structure boundaries must be ordered within the audible region",
        ));
    }
    if !analysis.intro_confidence.is_finite()
        || !(0.0..=1.0).contains(&analysis.intro_confidence)
        || !analysis.outro_confidence.is_finite()
        || !(0.0..=1.0).contains(&analysis.outro_confidence)
        || (analysis.intro_end.is_none() && analysis.intro_confidence != 0.0)
        || (analysis.outro_start.is_none() && analysis.outro_confidence != 0.0)
    {
        return Err(AnalysisCacheError::InvalidAnalysis(
            "structure confidences must match boundaries and be between zero and one",
        ));
    }
    let expected_vocal_bins =
        analysis.duration.as_secs_f64() * f64::from(analysis.vocal_activity_rate);
    let minimum_vocal_bins = expected_vocal_bins.floor() as usize;
    // The source-rate analyzer can emit one final partial bin, while the track
    // duration is measured from the downsampled analysis stream. Allow one bin
    // of rounding tolerance in that direction.
    let maximum_vocal_bins = expected_vocal_bins.ceil() as usize + 1;
    if (analysis.vocal_activity_rate == 0
        && (!analysis.vocal_activity.is_empty() || !analysis.vocal_activity_confidences.is_empty()))
        || (analysis.vocal_activity_rate > 0
            && (analysis.vocal_activity_rate > 8
                || analysis.vocal_activity.is_empty()
                || analysis.vocal_activity.len() != analysis.vocal_activity_confidences.len()
                || analysis.vocal_activity.len() < minimum_vocal_bins
                || analysis.vocal_activity.len() > maximum_vocal_bins))
    {
        return Err(AnalysisCacheError::InvalidAnalysis(
            "vocal activity profile must have a valid rate, length, and confidence shape",
        ));
    }
    let expected_energy_bins =
        analysis.duration.as_secs_f64() * f64::from(analysis.energy_profile_rate);
    let minimum_energy_bins = expected_energy_bins.floor() as usize;
    let maximum_energy_bins = expected_energy_bins.ceil() as usize + 1;
    if (analysis.energy_profile_rate == 0 && !analysis.energy_profile.is_empty())
        || (analysis.energy_profile_rate > 0
            && (analysis.energy_profile_rate > 8
                || analysis.energy_profile.is_empty()
                || analysis.energy_profile.len() < minimum_energy_bins
                || analysis.energy_profile.len() > maximum_energy_bins))
    {
        return Err(AnalysisCacheError::InvalidAnalysis(
            "energy profile must have a valid rate and length",
        ));
    }
    if analysis
        .bpm
        .is_some_and(|bpm| !bpm.is_finite() || bpm <= 0.0)
    {
        return Err(AnalysisCacheError::InvalidAnalysis(
            "BPM must be finite and greater than zero",
        ));
    }
    if !analysis.beat_confidence.is_finite() || !(0.0..=1.0).contains(&analysis.beat_confidence) {
        return Err(AnalysisCacheError::InvalidAnalysis(
            "beat confidence must be between zero and one",
        ));
    }
    if !analysis.downbeat_confidence.is_finite()
        || !(0.0..=1.0).contains(&analysis.downbeat_confidence)
    {
        return Err(AnalysisCacheError::InvalidAnalysis(
            "downbeat confidence must be between zero and one",
        ));
    }
    if analysis
        .first_beat
        .is_some_and(|beat| beat > analysis.duration)
    {
        return Err(AnalysisCacheError::InvalidAnalysis(
            "first beat must be within the track duration",
        ));
    }
    if analysis
        .beat_markers
        .iter()
        .any(|beat| *beat > analysis.duration)
        || analysis
            .beat_markers
            .windows(2)
            .any(|beats| beats[0] >= beats[1])
    {
        return Err(AnalysisCacheError::InvalidAnalysis(
            "beat markers must be ordered within the track duration",
        ));
    }
    if !analysis.beat_marker_confidences.is_empty()
        && (analysis.beat_marker_confidences.len() != analysis.beat_markers.len()
            || analysis
                .beat_marker_confidences
                .iter()
                .any(|confidence| !confidence.is_finite() || !(0.0..=1.0).contains(confidence)))
    {
        return Err(AnalysisCacheError::InvalidAnalysis(
            "beat marker confidences must match markers and be between zero and one",
        ));
    }
    if analysis
        .first_downbeat
        .is_some_and(|downbeat| downbeat > analysis.duration)
    {
        return Err(AnalysisCacheError::InvalidAnalysis(
            "first downbeat must be within the track duration",
        ));
    }
    if analysis.musical_key.is_some_and(|key| {
        key.tonic >= 12 || !key.confidence.is_finite() || !(0.0..=1.0).contains(&key.confidence)
    }) {
        return Err(AnalysisCacheError::InvalidAnalysis(
            "musical key must have a valid tonic and confidence",
        ));
    }
    if analysis.rms_dbfs.is_some_and(|value| !value.is_finite())
        || analysis
            .sample_peak_dbfs
            .is_some_and(|value| !value.is_finite())
        || analysis
            .integrated_lufs
            .is_some_and(|value| !value.is_finite())
        || analysis
            .true_peak_dbtp
            .is_some_and(|value| !value.is_finite())
    {
        return Err(AnalysisCacheError::InvalidAnalysis(
            "level measurements must be finite",
        ));
    }
    Ok(())
}

#[derive(Debug)]
struct CachedAnalysisV2 {
    schema_version: u32,
    analyzer_version: String,
    provider_id: String,
    canonical_key: String,
    content_length: Option<u64>,
    expected_duration_micros: Option<u64>,
    analysis: TrackAnalysisV2,
}

impl CachedAnalysisV2 {
    fn new(key: &AnalysisCacheKey, analyzer_version: &str, analysis: &TrackAnalysisV2) -> Self {
        Self {
            schema_version: ANALYSIS_CACHE_V2_SCHEMA_VERSION,
            analyzer_version: analyzer_version.to_owned(),
            provider_id: key.provider_id.clone(),
            canonical_key: key.canonical_key.clone(),
            content_length: key.content_length,
            expected_duration_micros: key.expected_duration_micros,
            analysis: analysis.clone(),
        }
    }

    fn matches(&self, key: &AnalysisCacheKey, analyzer_version: &str) -> bool {
        self.schema_version == ANALYSIS_CACHE_V2_SCHEMA_VERSION
            && self.analyzer_version == analyzer_version
            && self.provider_id == key.provider_id
            && self.canonical_key == key.canonical_key
            && optional_identity_matches(self.content_length, key.content_length, 0)
            && optional_identity_matches(
                self.expected_duration_micros,
                key.expected_duration_micros,
                SOURCE_DURATION_TOLERANCE_MICROS,
            )
    }
}

const V2_MAGIC: [u8; 4] = *b"WAM2";
const V2_UNKNOWN_SCORE: u8 = ANALYSIS_CACHE_V2_UNKNOWN_SCORE;
const V2_MAX_ITEMS: usize = 250_000;

/// Validate the domain record plus the cross-component bounds which cannot be
/// expressed by an individual V2 value type (for example cue/section indexes
/// and beat timestamps relative to the track duration).
fn validate_analysis_v2(analysis: &TrackAnalysisV2) -> Result<(), AnalysisCacheError> {
    if analysis.duration.is_zero() {
        return Err(AnalysisCacheError::InvalidAnalysis(
            "V2 duration must be greater than zero",
        ));
    }
    if analysis.duration.as_micros() > u128::from(u64::MAX)
        || analysis.audible_start.as_micros() > u128::from(u64::MAX)
        || analysis.audible_end.as_micros() > u128::from(u64::MAX)
    {
        return Err(AnalysisCacheError::InvalidAnalysis(
            "V2 duration exceeds the encoded range",
        ));
    }
    if !analysis.validate() {
        return Err(AnalysisCacheError::InvalidAnalysis(
            "V2 analysis contains an invalid value",
        ));
    }
    if analysis.audible_start > analysis.audible_end || analysis.audible_end > analysis.duration {
        return Err(AnalysisCacheError::InvalidAnalysis(
            "V2 audible boundaries must be ordered within duration",
        ));
    }
    {
        let rhythm = &analysis.rhythm;
        if rhythm.beats.len() > V2_MAX_ITEMS
            || rhythm.tempo_hypotheses.len() > V2_MAX_ITEMS
            || rhythm.meter_hypotheses.len() > V2_MAX_ITEMS
        {
            return Err(AnalysisCacheError::InvalidAnalysis(
                "V2 rhythm arrays exceed the bounded cache limit",
            ));
        }
        if rhythm.beats.iter().any(|beat| {
            beat.time > analysis.duration
                || beat.time.as_nanos() % 1_000_000 != 0
                || u32::try_from(beat.time.as_millis()).is_err()
        }) || rhythm
            .beats
            .windows(2)
            .any(|window| window[0].time.as_millis() >= window[1].time.as_millis())
        {
            return Err(AnalysisCacheError::InvalidAnalysis(
                "V2 beat timestamps must be ordered, millisecond-aligned, and within duration",
            ));
        }
        let structure = &analysis.structure;
        structure
            .validate_for_beat_count(rhythm.beats.len())
            .map_err(|_| {
                AnalysisCacheError::InvalidAnalysis(
                    "V2 structure section/cue indexes must reference beats",
                )
            })?;
    }
    {
        let structure = &analysis.structure;
        if structure.sections.len() > V2_MAX_ITEMS
            || structure.phrase_boundaries.len() > V2_MAX_ITEMS
        {
            return Err(AnalysisCacheError::InvalidAnalysis(
                "V2 structure arrays exceed the bounded cache limit",
            ));
        }
    }
    if analysis.tonal.local_windows.len() > V2_MAX_ITEMS {
        return Err(AnalysisCacheError::InvalidAnalysis(
            "V2 local tonal windows exceed the bounded cache limit",
        ));
    }
    if analysis.tonal.local_windows.iter().any(|window| {
        window.end > analysis.duration
            || window.start.as_micros() > u128::from(u64::MAX)
            || window.end.as_micros() > u128::from(u64::MAX)
    }) {
        return Err(AnalysisCacheError::InvalidAnalysis(
            "V2 local tonal windows must fit the track duration",
        ));
    }
    if analysis.cues.len() > V2_MAX_ITEMS {
        return Err(AnalysisCacheError::InvalidAnalysis(
            "V2 cue array exceeds the bounded cache limit",
        ));
    }
    let beat_count = analysis.rhythm.beats.len();
    if analysis
        .cues
        .iter()
        .any(|cue| cue.validate_for_beat_count(beat_count).is_err())
    {
        return Err(AnalysisCacheError::InvalidAnalysis(
            "V2 cues must reference valid beat indexes",
        ));
    }
    validate_profile_shape(
        analysis.vocal.activity.len(),
        analysis.vocal.confidences.len(),
        analysis.vocal.rate_hz,
    )?;
    validate_profile_shape(
        analysis.energy.profile.len(),
        analysis.energy.confidences.len(),
        analysis.energy.rate_hz,
    )?;
    if !analysis.provenance.validate() || analysis.provenance.analyzer.trim().is_empty() {
        return Err(AnalysisCacheError::InvalidAnalysis(
            "V2 provenance must contain a non-empty analyzer identity",
        ));
    }
    if analysis.provenance.components.len() > V2_MAX_ITEMS {
        return Err(AnalysisCacheError::InvalidAnalysis(
            "V2 provenance components exceed the bounded cache limit",
        ));
    }
    Ok(())
}

fn validate_profile_shape(
    values: usize,
    confidences: usize,
    rate_hz: u16,
) -> Result<(), AnalysisCacheError> {
    if rate_hz == 0 || values != confidences || values > V2_MAX_ITEMS {
        return Err(AnalysisCacheError::InvalidAnalysis(
            "V2 profile rate and arrays have an invalid shape",
        ));
    }
    // An empty profile is the explicit "not analyzed" state; its non-zero
    // default rate must not force an allocation proportional to duration.
    if values == 0 {
        return Ok(());
    }
    Ok(())
}

#[derive(Default)]
struct BinaryWriter {
    bytes: Vec<u8>,
}

impl BinaryWriter {
    fn new() -> Self {
        Self {
            bytes: Vec::with_capacity(1024),
        }
    }

    fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    fn put_u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn put_u16(&mut self, value: u16) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn put_u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn put_u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn put_f32(&mut self, value: f32) {
        self.put_u32(value.to_bits());
    }

    fn put_string(&mut self, value: &str) -> Result<(), String> {
        let length = u32::try_from(value.len()).map_err(|_| "string is too long".to_owned())?;
        self.put_u32(length);
        self.bytes.extend_from_slice(value.as_bytes());
        Ok(())
    }

    fn put_optional_u64(&mut self, value: Option<u64>) {
        match value {
            Some(value) => {
                self.put_u8(1);
                self.put_u64(value);
            }
            None => self.put_u8(0),
        }
    }

    fn put_optional_string(&mut self, value: Option<&str>) -> Result<(), String> {
        match value {
            Some(value) => {
                self.put_u8(1);
                self.put_string(value)?;
            }
            None => self.put_u8(0),
        }
        Ok(())
    }

    fn put_optional_f32(&mut self, value: Option<f32>) {
        match value {
            Some(value) => {
                self.put_u8(1);
                self.put_f32(value);
            }
            None => self.put_u8(0),
        }
    }
}

struct BinaryReader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> BinaryReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], String> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or_else(|| "binary length overflow".to_owned())?;
        if end > self.bytes.len() {
            return Err("truncated V2 cache record".to_owned());
        }
        let value = &self.bytes[self.offset..end];
        self.offset = end;
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, String> {
        let bytes = self.take(2)?;
        let bytes = <[u8; 2]>::try_from(bytes).map_err(|_| "invalid u16".to_owned())?;
        Ok(u16::from_le_bytes(bytes))
    }

    fn u32(&mut self) -> Result<u32, String> {
        let bytes = self.take(4)?;
        let bytes = <[u8; 4]>::try_from(bytes).map_err(|_| "invalid u32".to_owned())?;
        Ok(u32::from_le_bytes(bytes))
    }

    fn u64(&mut self) -> Result<u64, String> {
        let bytes = self.take(8)?;
        let bytes = <[u8; 8]>::try_from(bytes).map_err(|_| "invalid u64".to_owned())?;
        Ok(u64::from_le_bytes(bytes))
    }

    fn f32(&mut self) -> Result<f32, String> {
        Ok(f32::from_bits(self.u32()?))
    }

    fn string(&mut self) -> Result<String, String> {
        let length =
            usize::try_from(self.u32()?).map_err(|_| "string length overflow".to_owned())?;
        if length > ANALYSIS_CACHE_V2_MAX_FILE_BYTES as usize {
            return Err("string exceeds V2 cache bound".to_owned());
        }
        let bytes = self.take(length)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| "invalid UTF-8 in V2 cache".to_owned())
    }

    fn optional_u64(&mut self) -> Result<Option<u64>, String> {
        match self.u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.u64()?)),
            _ => Err("invalid optional value marker".to_owned()),
        }
    }

    fn optional_string(&mut self) -> Result<Option<String>, String> {
        match self.u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.string()?)),
            _ => Err("invalid optional value marker".to_owned()),
        }
    }

    fn optional_f32(&mut self) -> Result<Option<f32>, String> {
        match self.u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.f32()?)),
            _ => Err("invalid optional f32 marker".to_owned()),
        }
    }

    fn count(&mut self, label: &str) -> Result<usize, String> {
        let count = usize::try_from(self.u32()?).map_err(|_| format!("{label} count overflow"))?;
        if count > V2_MAX_ITEMS {
            return Err(format!("{label} count exceeds bounded cache limit"));
        }
        Ok(count)
    }

    fn done(&self) -> bool {
        self.offset == self.bytes.len()
    }
}

fn encode_v2_record(record: &CachedAnalysisV2) -> Result<Vec<u8>, String> {
    let mut writer = BinaryWriter::new();
    writer.bytes.extend_from_slice(&V2_MAGIC);
    writer.put_u32(record.schema_version);
    writer.put_string(&record.analyzer_version)?;
    writer.put_string(&record.provider_id)?;
    writer.put_string(&record.canonical_key)?;
    writer.put_optional_u64(record.content_length);
    writer.put_optional_u64(record.expected_duration_micros);
    encode_track_analysis_v2(&mut writer, &record.analysis)?;
    Ok(writer.into_bytes())
}

fn decode_v2_record(bytes: &[u8]) -> Result<CachedAnalysisV2, String> {
    let mut reader = BinaryReader::new(bytes);
    if reader.take(V2_MAGIC.len())? != V2_MAGIC {
        return Err("invalid V2 cache magic".to_owned());
    }
    let record = CachedAnalysisV2 {
        schema_version: reader.u32()?,
        analyzer_version: reader.string()?,
        provider_id: reader.string()?,
        canonical_key: reader.string()?,
        content_length: reader.optional_u64()?,
        expected_duration_micros: reader.optional_u64()?,
        analysis: decode_track_analysis_v2(&mut reader)?,
    };
    if !reader.done() {
        return Err("trailing bytes in V2 cache record".to_owned());
    }
    Ok(record)
}

fn encode_track_analysis_v2(
    writer: &mut BinaryWriter,
    analysis: &TrackAnalysisV2,
) -> Result<(), String> {
    writer.put_u64(duration_to_micros(analysis.duration));
    writer.put_u64(duration_to_micros(analysis.audible_start));
    writer.put_u64(duration_to_micros(analysis.audible_end));
    encode_rhythm(writer, &analysis.rhythm)?;
    encode_structure(writer, &analysis.structure)?;
    encode_tonal(writer, &analysis.tonal)?;
    encode_vocal(writer, &analysis.vocal)?;
    encode_energy(writer, &analysis.energy)?;
    put_count(writer, analysis.cues.len(), "cue")?;
    for cue in &analysis.cues {
        encode_cue(writer, cue)?;
    }
    encode_provenance(writer, &analysis.provenance)
}

fn decode_track_analysis_v2(reader: &mut BinaryReader<'_>) -> Result<TrackAnalysisV2, String> {
    let duration = Duration::from_micros(reader.u64()?);
    let audible_start = Duration::from_micros(reader.u64()?);
    let audible_end = Duration::from_micros(reader.u64()?);
    let rhythm = decode_rhythm(reader)?;
    let structure = decode_structure(reader)?;
    let tonal = decode_tonal(reader)?;
    let vocal = decode_vocal(reader)?;
    let energy = decode_energy(reader)?;
    let cue_count = reader.count("cue")?;
    let mut cues = Vec::with_capacity(cue_count);
    for _ in 0..cue_count {
        cues.push(decode_cue(reader)?);
    }
    let provenance = decode_provenance(reader)?;
    Ok(TrackAnalysisV2 {
        duration,
        audible_start,
        audible_end,
        rhythm,
        structure,
        tonal,
        vocal,
        energy,
        cues,
        provenance,
    })
}

fn encode_rhythm(writer: &mut BinaryWriter, rhythm: &RhythmAnalysis) -> Result<(), String> {
    // Keep BeatEvent values in parallel arrays. The order is part of the
    // codec contract: beat_times_ms, beat_model_score, downbeat_model_score,
    // timing_confidence, onset_support, and low_frequency_support. A missing
    // score is encoded as 255; 0..=254 are the quantized [0, 1] range. In
    // particular, missing is not silently converted to zero-confidence
    // evidence.
    put_count(writer, rhythm.beats.len(), "beat")?;
    for beat in &rhythm.beats {
        let milliseconds = beat.time.as_millis();
        let milliseconds = u32::try_from(milliseconds)
            .map_err(|_| "beat timestamp does not fit beat_times_ms".to_owned())?;
        writer.put_u32(milliseconds);
    }
    for beat in &rhythm.beats {
        writer.put_u8(encode_optional_score(
            beat.beat_model_score.map(ModelScore::get),
        ));
    }
    for beat in &rhythm.beats {
        writer.put_u8(encode_optional_score(
            beat.downbeat_model_score.map(ModelScore::get),
        ));
    }
    for beat in &rhythm.beats {
        writer.put_u8(encode_score(beat.timing_confidence.get()));
    }
    for beat in &rhythm.beats {
        writer.put_u8(encode_optional_score(beat.onset_support.map(Support::get)));
    }
    for beat in &rhythm.beats {
        writer.put_u8(encode_optional_score(
            beat.low_frequency_support.map(Support::get),
        ));
    }

    put_count(writer, rhythm.tempo_hypotheses.len(), "tempo hypothesis")?;
    for hypothesis in &rhythm.tempo_hypotheses {
        writer.put_f32(hypothesis.bpm);
        writer.put_f32(hypothesis.relative_weight.get());
        writer.put_u8(encode_relation(hypothesis.relation));
    }
    put_count(writer, rhythm.meter_hypotheses.len(), "meter hypothesis")?;
    for hypothesis in &rhythm.meter_hypotheses {
        writer.put_u8(hypothesis.beats_per_bar);
        writer.put_u8(hypothesis.downbeat_phase);
        writer.put_f32(hypothesis.score.get());
    }
    Ok(())
}

fn decode_rhythm(reader: &mut BinaryReader<'_>) -> Result<RhythmAnalysis, String> {
    let beat_count = reader.count("beat")?;
    let mut beat_times_ms = Vec::with_capacity(beat_count);
    for _ in 0..beat_count {
        beat_times_ms.push(reader.u32()?);
    }
    let mut beat_model_scores = Vec::with_capacity(beat_count);
    for _ in 0..beat_count {
        beat_model_scores.push(decode_optional_score(reader.u8()?)?);
    }
    let mut downbeat_model_scores = Vec::with_capacity(beat_count);
    for _ in 0..beat_count {
        downbeat_model_scores.push(decode_optional_score(reader.u8()?)?);
    }
    let mut timing_confidences = Vec::with_capacity(beat_count);
    for _ in 0..beat_count {
        timing_confidences.push(decode_required_confidence(reader.u8()?)?);
    }
    let mut onset_supports = Vec::with_capacity(beat_count);
    for _ in 0..beat_count {
        onset_supports.push(decode_optional_support(reader.u8()?)?);
    }
    let mut low_frequency_supports = Vec::with_capacity(beat_count);
    for _ in 0..beat_count {
        low_frequency_supports.push(decode_optional_support(reader.u8()?)?);
    }
    let beats = beat_times_ms
        .into_iter()
        .zip(beat_model_scores)
        .zip(downbeat_model_scores)
        .zip(timing_confidences)
        .zip(onset_supports)
        .zip(low_frequency_supports)
        .map(
            |(
                (
                    (((beat_time_ms, beat_model_score), downbeat_model_score), timing_confidence),
                    onset_support,
                ),
                low_frequency_support,
            )| {
                wotoha_core::analysis::BeatEvent::new(
                    Duration::from_millis(u64::from(beat_time_ms)),
                    beat_model_score,
                    downbeat_model_score,
                    timing_confidence,
                    onset_support,
                    low_frequency_support,
                )
            },
        )
        .collect();

    let tempo_count = reader.count("tempo hypothesis")?;
    let mut tempo_hypotheses = Vec::with_capacity(tempo_count);
    for _ in 0..tempo_count {
        tempo_hypotheses.push(wotoha_core::analysis::TempoHypothesis {
            bpm: reader.f32()?,
            relative_weight: decode_unit_f32(reader.f32()?)?,
            relation: decode_relation(reader.u8()?)?,
        });
    }
    let meter_count = reader.count("meter hypothesis")?;
    let mut meter_hypotheses = Vec::with_capacity(meter_count);
    for _ in 0..meter_count {
        meter_hypotheses.push(MeterHypothesis {
            beats_per_bar: reader.u8()?,
            downbeat_phase: reader.u8()?,
            score: decode_unit_f32(reader.f32()?)?,
        });
    }
    Ok(RhythmAnalysis {
        beats,
        tempo_hypotheses,
        meter_hypotheses,
    })
}

fn put_count(writer: &mut BinaryWriter, count: usize, label: &str) -> Result<(), String> {
    if count > V2_MAX_ITEMS {
        return Err(format!("{label} count exceeds bounded cache limit"));
    }
    writer.put_u32(u32::try_from(count).map_err(|_| format!("{label} count overflow"))?);
    Ok(())
}

fn encode_structure(
    writer: &mut BinaryWriter,
    structure: &StructureAnalysis,
) -> Result<(), String> {
    put_count(writer, structure.sections.len(), "section")?;
    for section in &structure.sections {
        writer.put_u32(
            u32::try_from(section.start_beat).map_err(|_| "section index overflow".to_owned())?,
        );
        writer.put_u32(
            u32::try_from(section.end_beat).map_err(|_| "section index overflow".to_owned())?,
        );
        writer.put_f32(section.boundary_confidence.get());
        put_count(writer, section.labels.len(), "section label")?;
        for label in &section.labels {
            writer.put_u8(encode_section_label(label.label));
            writer.put_f32(label.score.get());
        }
    }
    put_count(writer, structure.phrase_boundaries.len(), "phrase boundary")?;
    for boundary in &structure.phrase_boundaries {
        writer.put_u32(
            u32::try_from(boundary.beat_index).map_err(|_| "phrase index overflow".to_owned())?,
        );
        writer.put_f32(boundary.strength.get());
        writer.put_u8(encode_phrase_boundary_source(boundary.source));
    }
    Ok(())
}

fn decode_structure(reader: &mut BinaryReader<'_>) -> Result<StructureAnalysis, String> {
    let section_count = reader.count("section")?;
    let mut sections = Vec::with_capacity(section_count);
    for _ in 0..section_count {
        let start_beat =
            usize::try_from(reader.u32()?).map_err(|_| "section index overflow".to_owned())?;
        let end_beat =
            usize::try_from(reader.u32()?).map_err(|_| "section index overflow".to_owned())?;
        let boundary_confidence = decode_confidence_f32(reader.f32()?)?;
        let label_count = reader.count("section label")?;
        let mut labels = Vec::with_capacity(label_count);
        for _ in 0..label_count {
            labels.push(SectionLabelScore {
                label: decode_section_label(reader.u8()?)?,
                score: decode_unit_f32(reader.f32()?)?,
            });
        }
        sections.push(Section {
            start_beat,
            end_beat,
            boundary_confidence,
            labels,
        });
    }
    let boundary_count = reader.count("phrase boundary")?;
    let mut phrase_boundaries = Vec::with_capacity(boundary_count);
    for _ in 0..boundary_count {
        phrase_boundaries.push(PhraseBoundary {
            beat_index: usize::try_from(reader.u32()?)
                .map_err(|_| "phrase index overflow".to_owned())?,
            strength: decode_unit_f32(reader.f32()?)?,
            source: decode_phrase_boundary_source(reader.u8()?)?,
        });
    }
    Ok(StructureAnalysis {
        sections,
        phrase_boundaries,
    })
}

fn encode_tonal(writer: &mut BinaryWriter, tonal: &TonalAnalysis) -> Result<(), String> {
    encode_optional_key(writer, tonal.global_key);
    put_count(writer, tonal.local_windows.len(), "local tonal window")?;
    for window in &tonal.local_windows {
        writer.put_u64(duration_to_micros(window.start));
        writer.put_u64(duration_to_micros(window.end));
        encode_optional_key(writer, window.key);
        writer.put_optional_f32(window.confidence.map(Confidence::get));
    }
    put_count(writer, tonal.alternatives.len(), "tonal alternative")?;
    for key in &tonal.alternatives {
        encode_key(writer, *key);
    }
    Ok(())
}

fn decode_tonal(reader: &mut BinaryReader<'_>) -> Result<TonalAnalysis, String> {
    let global_key = decode_optional_key(reader)?;
    let local_window_count = reader.count("local tonal window")?;
    let mut local_windows = Vec::with_capacity(local_window_count);
    for _ in 0..local_window_count {
        local_windows.push(LocalTonalWindow {
            start: Duration::from_micros(reader.u64()?),
            end: Duration::from_micros(reader.u64()?),
            key: decode_optional_key(reader)?,
            confidence: reader
                .optional_f32()?
                .map(decode_confidence_f32)
                .transpose()?,
        });
    }
    let alternative_count = reader.count("tonal alternative")?;
    let mut alternatives = Vec::with_capacity(alternative_count);
    for _ in 0..alternative_count {
        alternatives.push(decode_key(reader)?);
    }
    Ok(TonalAnalysis {
        global_key,
        local_windows,
        alternatives,
    })
}

fn encode_key(writer: &mut BinaryWriter, key: V2MusicalKey) {
    writer.put_u8(key.tonic);
    writer.put_u8(match key.mode {
        V2KeyMode::Major => 0,
        V2KeyMode::Minor => 1,
    });
    writer.put_f32(key.confidence.get());
}

fn encode_optional_key(writer: &mut BinaryWriter, key: Option<V2MusicalKey>) {
    match key {
        Some(key) => {
            writer.put_u8(1);
            encode_key(writer, key);
        }
        None => writer.put_u8(0),
    }
}

fn decode_key(reader: &mut BinaryReader<'_>) -> Result<V2MusicalKey, String> {
    let tonic = reader.u8()?;
    let mode = match reader.u8()? {
        0 => V2KeyMode::Major,
        1 => V2KeyMode::Minor,
        _ => return Err("invalid key mode".to_owned()),
    };
    Ok(V2MusicalKey {
        tonic,
        mode,
        confidence: decode_confidence_f32(reader.f32()?)?,
    })
}

fn decode_optional_key(reader: &mut BinaryReader<'_>) -> Result<Option<V2MusicalKey>, String> {
    match reader.u8()? {
        0 => Ok(None),
        1 => Ok(Some(decode_key(reader)?)),
        _ => Err("invalid key option marker".to_owned()),
    }
}

fn encode_vocal(writer: &mut BinaryWriter, vocal: &VocalAnalysis) -> Result<(), String> {
    writer.put_u16(vocal.rate_hz);
    put_count(writer, vocal.activity.len(), "vocal profile")?;
    for value in &vocal.activity {
        writer.put_f32(value.get());
    }
    for value in &vocal.confidences {
        writer.put_f32(value.get());
    }
    Ok(())
}

fn decode_vocal(reader: &mut BinaryReader<'_>) -> Result<VocalAnalysis, String> {
    let rate_hz = reader.u16()?;
    let count = reader.count("vocal profile")?;
    let mut activity = Vec::with_capacity(count);
    for _ in 0..count {
        activity.push(decode_unit_f32(reader.f32()?)?);
    }
    let mut confidences = Vec::with_capacity(count);
    for _ in 0..count {
        confidences.push(decode_confidence_f32(reader.f32()?)?);
    }
    Ok(VocalAnalysis {
        activity,
        confidences,
        rate_hz,
    })
}

fn encode_energy(writer: &mut BinaryWriter, energy: &EnergyAnalysis) -> Result<(), String> {
    writer.put_u16(energy.rate_hz);
    put_count(writer, energy.profile.len(), "energy profile")?;
    for value in &energy.profile {
        writer.put_f32(value.get());
    }
    for value in &energy.confidences {
        writer.put_f32(value.get());
    }
    writer.put_optional_f32(energy.rms_dbfs);
    writer.put_optional_f32(energy.sample_peak_dbfs);
    writer.put_optional_f32(energy.integrated_lufs);
    writer.put_optional_f32(energy.true_peak_dbtp);
    Ok(())
}

fn decode_energy(reader: &mut BinaryReader<'_>) -> Result<EnergyAnalysis, String> {
    let rate_hz = reader.u16()?;
    let count = reader.count("energy profile")?;
    let mut profile = Vec::with_capacity(count);
    for _ in 0..count {
        profile.push(decode_unit_f32(reader.f32()?)?);
    }
    let mut confidences = Vec::with_capacity(count);
    for _ in 0..count {
        confidences.push(decode_confidence_f32(reader.f32()?)?);
    }
    Ok(EnergyAnalysis {
        profile,
        confidences,
        rate_hz,
        rms_dbfs: reader.optional_f32()?,
        sample_peak_dbfs: reader.optional_f32()?,
        integrated_lufs: reader.optional_f32()?,
        true_peak_dbtp: reader.optional_f32()?,
    })
}

fn encode_cue(writer: &mut BinaryWriter, cue: &DjCue) -> Result<(), String> {
    writer
        .put_u32(u32::try_from(cue.beat_index).map_err(|_| "cue beat index overflow".to_owned())?);
    writer.put_f32(cue.importance.get());
    writer.put_f32(cue.mix_in.get());
    writer.put_f32(cue.mix_out.get());
    writer.put_f32(cue.cut_safe.get());
    writer.put_f32(cue.phrase_boundary.get());
    writer.put_f32(cue.drop.get());
    writer.put_f32(cue.build.get());
    encode_cue_provenance(writer, cue.provenance)
}

fn decode_cue(reader: &mut BinaryReader<'_>) -> Result<DjCue, String> {
    let beat_index =
        usize::try_from(reader.u32()?).map_err(|_| "cue beat index overflow".to_owned())?;
    let importance = decode_unit_f32(reader.f32()?)?;
    let mix_in = decode_unit_f32(reader.f32()?)?;
    let mix_out = decode_unit_f32(reader.f32()?)?;
    let cut_safe = decode_unit_f32(reader.f32()?)?;
    let phrase_boundary = decode_unit_f32(reader.f32()?)?;
    let drop = decode_unit_f32(reader.f32()?)?;
    let build = decode_unit_f32(reader.f32()?)?;
    let provenance = decode_cue_provenance(reader.u8()?, reader)?;
    Ok(DjCue {
        beat_index,
        importance,
        mix_in,
        mix_out,
        cut_safe,
        phrase_boundary,
        drop,
        build,
        provenance,
    })
}

fn encode_provenance(
    writer: &mut BinaryWriter,
    provenance: &AnalysisProvenance,
) -> Result<(), String> {
    writer.put_string(&provenance.analyzer)?;
    writer.put_optional_string(provenance.schema_version.as_deref())?;
    put_count(writer, provenance.components.len(), "provenance component")?;
    for component in &provenance.components {
        encode_component_provenance(writer, component)?;
    }
    encode_optional_component(writer, provenance.rhythm.as_ref())?;
    encode_optional_component(writer, provenance.structure.as_ref())?;
    encode_optional_component(writer, provenance.tonal.as_ref())?;
    encode_optional_component(writer, provenance.vocal.as_ref())?;
    encode_optional_component(writer, provenance.energy.as_ref())?;
    encode_optional_component(writer, provenance.cue.as_ref())?;
    encode_optional_component(writer, provenance.cues.as_ref())?;
    match provenance.overall_confidence {
        Some(confidence) => {
            writer.put_u8(1);
            writer.put_f32(confidence.get());
        }
        None => writer.put_u8(0),
    }
    Ok(())
}

fn decode_provenance(reader: &mut BinaryReader<'_>) -> Result<AnalysisProvenance, String> {
    let analyzer = reader.string()?;
    let schema_version = reader.optional_string()?;
    let component_count = reader.count("provenance component")?;
    let mut components = Vec::with_capacity(component_count);
    for _ in 0..component_count {
        components.push(decode_component_provenance(reader)?);
    }
    let rhythm = decode_optional_component(reader)?;
    let structure = decode_optional_component(reader)?;
    let tonal = decode_optional_component(reader)?;
    let vocal = decode_optional_component(reader)?;
    let energy = decode_optional_component(reader)?;
    let cue = decode_optional_component(reader)?;
    let cues = decode_optional_component(reader)?;
    let overall_confidence = match reader.u8()? {
        0 => None,
        1 => Some(decode_unit_f32(reader.f32()?)?),
        _ => return Err("invalid overall confidence option marker".to_owned()),
    };
    Ok(AnalysisProvenance {
        analyzer,
        schema_version,
        components,
        rhythm,
        structure,
        tonal,
        vocal,
        energy,
        cue,
        cues,
        overall_confidence,
    })
}

fn encode_component_provenance(
    writer: &mut BinaryWriter,
    component: &ComponentProvenance,
) -> Result<(), String> {
    writer.put_string(&component.component)?;
    writer.put_u8(encode_analysis_method(&component.method));
    match &component.model {
        Some(model) => {
            writer.put_u8(1);
            writer.put_string(&model.id)?;
            writer.put_string(&model.version)?;
            writer.put_optional_string(model.revision.as_deref())?;
        }
        None => writer.put_u8(0),
    }
    match component.confidence {
        Some(confidence) => {
            writer.put_u8(1);
            writer.put_f32(confidence.get());
        }
        None => writer.put_u8(0),
    }
    writer.put_optional_string(component.notes.as_deref())
}

fn decode_component_provenance(
    reader: &mut BinaryReader<'_>,
) -> Result<ComponentProvenance, String> {
    let component = reader.string()?;
    let method = decode_analysis_method(reader.u8()?)?;
    let model = match reader.u8()? {
        0 => None,
        1 => Some(ModelIdentity {
            id: reader.string()?,
            version: reader.string()?,
            revision: reader.optional_string()?,
        }),
        _ => return Err("invalid model option marker".to_owned()),
    };
    let confidence = match reader.u8()? {
        0 => None,
        1 => Some(decode_confidence_f32(reader.f32()?)?),
        _ => return Err("invalid component confidence option marker".to_owned()),
    };
    let notes = reader.optional_string()?;
    Ok(ComponentProvenance {
        component,
        method,
        model,
        confidence,
        notes,
    })
}

fn encode_optional_component(
    writer: &mut BinaryWriter,
    component: Option<&ComponentProvenance>,
) -> Result<(), String> {
    match component {
        Some(component) => {
            writer.put_u8(1);
            encode_component_provenance(writer, component)?;
        }
        None => writer.put_u8(0),
    }
    Ok(())
}

fn decode_optional_component(
    reader: &mut BinaryReader<'_>,
) -> Result<Option<ComponentProvenance>, String> {
    match reader.u8()? {
        0 => Ok(None),
        1 => Ok(Some(decode_component_provenance(reader)?)),
        _ => Err("invalid provenance component option marker".to_owned()),
    }
}

fn encode_score(value: f32) -> u8 {
    // 255 is reserved for `None`; [0, 1] maps to [0, 254].
    (value.clamp(0.0, 1.0) * f32::from(V2_UNKNOWN_SCORE - 1)).round() as u8
}

fn encode_optional_score(value: Option<f32>) -> u8 {
    value.map_or(V2_UNKNOWN_SCORE, encode_score)
}

fn decode_score(value: u8) -> Result<ModelScore, String> {
    if value == V2_UNKNOWN_SCORE {
        return Err("unknown score used for a required value".to_owned());
    }
    ModelScore::new(f32::from(value) / f32::from(V2_UNKNOWN_SCORE - 1))
        .ok_or_else(|| "invalid model score".to_owned())
}

fn decode_optional_score(value: u8) -> Result<Option<ModelScore>, String> {
    (value != V2_UNKNOWN_SCORE)
        .then(|| decode_score(value))
        .transpose()
}

fn decode_optional_support(value: u8) -> Result<Option<Support>, String> {
    if value == V2_UNKNOWN_SCORE {
        return Ok(None);
    }
    Support::new(f32::from(value) / f32::from(V2_UNKNOWN_SCORE - 1))
        .map(Some)
        .ok_or_else(|| "invalid support".to_owned())
}

fn decode_required_confidence(value: u8) -> Result<Confidence, String> {
    if value == V2_UNKNOWN_SCORE {
        return Err("unknown confidence used for a required value".to_owned());
    }
    Confidence::new(f32::from(value) / f32::from(V2_UNKNOWN_SCORE - 1))
        .ok_or_else(|| "invalid confidence".to_owned())
}

fn decode_unit_f32(value: f32) -> Result<UnitInterval, String> {
    UnitInterval::new(value).ok_or_else(|| "invalid unit value".to_owned())
}

fn decode_confidence_f32(value: f32) -> Result<Confidence, String> {
    Confidence::new(value).ok_or_else(|| "invalid confidence".to_owned())
}

fn encode_relation(relation: Relation) -> u8 {
    match relation {
        Relation::Primary => 0,
        Relation::HalfTime => 1,
        Relation::DoubleTime => 2,
        Relation::Alternative => 3,
    }
}

fn decode_relation(value: u8) -> Result<Relation, String> {
    match value {
        0 => Ok(Relation::Primary),
        1 => Ok(Relation::HalfTime),
        2 => Ok(Relation::DoubleTime),
        3 => Ok(Relation::Alternative),
        _ => Err("invalid tempo relation".to_owned()),
    }
}

fn encode_analysis_method(method: &AnalysisMethod) -> u8 {
    match method {
        AnalysisMethod::Classical => 0,
        AnalysisMethod::Neural => 1,
        AnalysisMethod::Hybrid => 2,
        AnalysisMethod::Derived => 3,
        AnalysisMethod::Imported => 4,
        AnalysisMethod::Unknown => 5,
    }
}

fn decode_analysis_method(value: u8) -> Result<AnalysisMethod, String> {
    match value {
        0 => Ok(AnalysisMethod::Classical),
        1 => Ok(AnalysisMethod::Neural),
        2 => Ok(AnalysisMethod::Hybrid),
        3 => Ok(AnalysisMethod::Derived),
        4 => Ok(AnalysisMethod::Imported),
        5 => Ok(AnalysisMethod::Unknown),
        _ => Err("invalid analysis method".to_owned()),
    }
}

fn encode_section_label(label: SectionLabel) -> u8 {
    match label {
        SectionLabel::Intro => 0,
        SectionLabel::Verse => 1,
        SectionLabel::PreChorus => 2,
        SectionLabel::Chorus => 3,
        SectionLabel::Build => 4,
        SectionLabel::Drop => 5,
        SectionLabel::Breakdown => 6,
        SectionLabel::Bridge => 7,
        SectionLabel::Outro => 8,
        SectionLabel::Instrumental => 9,
        SectionLabel::Unknown => 10,
    }
}

fn decode_section_label(value: u8) -> Result<SectionLabel, String> {
    match value {
        0 => Ok(SectionLabel::Intro),
        1 => Ok(SectionLabel::Verse),
        2 => Ok(SectionLabel::PreChorus),
        3 => Ok(SectionLabel::Chorus),
        4 => Ok(SectionLabel::Build),
        5 => Ok(SectionLabel::Drop),
        6 => Ok(SectionLabel::Breakdown),
        7 => Ok(SectionLabel::Bridge),
        8 => Ok(SectionLabel::Outro),
        9 => Ok(SectionLabel::Instrumental),
        10 => Ok(SectionLabel::Unknown),
        _ => Err("invalid section label".to_owned()),
    }
}

fn encode_phrase_boundary_source(source: PhraseBoundarySource) -> u8 {
    match source {
        PhraseBoundarySource::Detected => 0,
        PhraseBoundarySource::Heuristic => 1,
        PhraseBoundarySource::PeriodicPrior => 2,
        PhraseBoundarySource::Imported => 3,
    }
}

fn decode_phrase_boundary_source(value: u8) -> Result<PhraseBoundarySource, String> {
    match value {
        0 => Ok(PhraseBoundarySource::Detected),
        1 => Ok(PhraseBoundarySource::Heuristic),
        2 => Ok(PhraseBoundarySource::PeriodicPrior),
        3 => Ok(PhraseBoundarySource::Imported),
        _ => Err("invalid phrase-boundary source".to_owned()),
    }
}

fn encode_cue_provenance(
    writer: &mut BinaryWriter,
    provenance: CueProvenance,
) -> Result<(), String> {
    match provenance {
        CueProvenance::Detected => writer.put_u8(0),
        CueProvenance::Heuristic => writer.put_u8(1),
        CueProvenance::PeriodicPrior => writer.put_u8(2),
        CueProvenance::Imported(source) => {
            writer.put_u8(3);
            writer.put_u8(encode_human_cue_source(source));
        }
        CueProvenance::Mixed => writer.put_u8(4),
    }
    Ok(())
}

fn decode_cue_provenance(
    value: u8,
    reader: &mut BinaryReader<'_>,
) -> Result<CueProvenance, String> {
    match value {
        0 => Ok(CueProvenance::Detected),
        1 => Ok(CueProvenance::Heuristic),
        2 => Ok(CueProvenance::PeriodicPrior),
        3 => Ok(CueProvenance::Imported(decode_human_cue_source(
            reader.u8()?,
        )?)),
        4 => Ok(CueProvenance::Mixed),
        _ => Err("invalid DJ cue provenance".to_owned()),
    }
}

fn encode_human_cue_source(source: HumanCueSource) -> u8 {
    match source {
        HumanCueSource::RekordboxMemory => 0,
        // Keep the legacy HotCue wire value stable. The explicit Rekordbox
        // spelling gets a new value so old records remain distinguishable.
        HumanCueSource::HotCue => 1,
        HumanCueSource::ManualWotoha => 2,
        HumanCueSource::RekordboxHotCue => 3,
    }
}

fn decode_human_cue_source(value: u8) -> Result<HumanCueSource, String> {
    match value {
        0 => Ok(HumanCueSource::RekordboxMemory),
        1 => Ok(HumanCueSource::HotCue),
        2 => Ok(HumanCueSource::ManualWotoha),
        3 => Ok(HumanCueSource::RekordboxHotCue),
        _ => Err("invalid human cue source".to_owned()),
    }
}

fn duration_to_micros(duration: Duration) -> u64 {
    duration.as_micros().min(u128::from(u64::MAX)) as u64
}

fn optional_identity_matches(left: Option<u64>, right: Option<u64>, tolerance: u64) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => left.abs_diff(right) <= tolerance,
        _ => true,
    }
}

fn update_length_prefixed(digest: &mut Sha256, value: &[u8]) {
    digest.update((value.len() as u64).to_le_bytes());
    digest.update(value);
}

fn replace_file(temp_path: &Path, destination: &Path) -> Result<(), AnalysisCacheError> {
    match fs::rename(temp_path, destination) {
        Ok(()) => Ok(()),
        Err(_source) if cfg!(windows) && destination.exists() => {
            fs::remove_file(destination).map_err(|source| {
                io_error("remove previous cache file", destination.to_owned(), source)
            })?;
            fs::rename(temp_path, destination)
                .map_err(|source| io_error("install cache file", destination.to_owned(), source))
        }
        Err(source) => Err(io_error(
            "install cache file",
            destination.to_owned(),
            source,
        )),
    }
}

fn io_error(operation: &'static str, path: PathBuf, source: std::io::Error) -> AnalysisCacheError {
    AnalysisCacheError::Io {
        operation,
        path,
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wotoha_core::analysis::{
        BeatEvent, Confidence, LocalTonalWindow, TempoHypothesis, TempoRelation,
    };

    fn analysis() -> TrackAnalysis {
        let mut vocal_activity = vec![20; 180 * 4];
        vocal_activity[12] = 200;
        TrackAnalysis {
            duration: Duration::from_secs(180),
            audible_start: Duration::from_millis(750),
            audible_end: Duration::from_millis(179_200),
            intro_end: Some(Duration::from_secs(8)),
            intro_confidence: 0.75,
            outro_start: Some(Duration::from_secs(170)),
            outro_confidence: 0.8,
            vocal_activity,
            vocal_activity_confidences: vec![230; 180 * 4],
            vocal_activity_rate: 4,
            energy_profile: vec![192; 180 * 4],
            energy_profile_rate: 4,
            bpm: Some(124.5),
            beat_confidence: 0.91,
            first_beat: Some(Duration::from_millis(750)),
            beat_markers: vec![Duration::from_millis(750)],
            beat_marker_confidences: vec![0.8],
            first_downbeat: Some(Duration::from_millis(750)),
            downbeat_confidence: 0.72,
            musical_key: Some(MusicalKey {
                tonic: 9,
                mode: KeyMode::Minor,
                confidence: 0.81,
            }),
            rms_dbfs: Some(-14.2),
            sample_peak_dbfs: Some(-1.0),
            integrated_lufs: Some(-13.7),
            true_peak_dbtp: Some(-0.8),
        }
    }

    #[test]
    fn stores_and_loads_analysis_without_exposing_source_key_in_filename() {
        let directory = TestDirectory::new();
        let cache = AnalysisCache::new(directory.path(), "tempo-v1").unwrap();
        let key = AnalysisCacheKey::new(
            "soundcloud",
            "artists/unsafe/../track",
            Some(42_000),
            Some(Duration::from_secs(180)),
        )
        .unwrap();

        cache.store(&key, &analysis()).unwrap();
        assert_eq!(cache.load(&key).unwrap(), Some(analysis()));

        let cache_path = cache.path_for(&key);
        let filename = cache_path.file_name().unwrap().to_string_lossy();
        assert_eq!(filename.len(), 69);
        assert!(!filename.contains("soundcloud"));
        assert!(!filename.contains("unsafe"));
    }

    #[test]
    fn overwrites_existing_analysis() {
        let directory = TestDirectory::new();
        let cache = AnalysisCache::new(directory.path(), "tempo-v1").unwrap();
        let key = AnalysisCacheKey::new("youtube", "abc", None, None).unwrap();
        let mut updated = analysis();
        updated.bpm = Some(128.0);

        cache.store(&key, &analysis()).unwrap();
        cache.store(&key, &updated).unwrap();

        assert_eq!(cache.load(&key).unwrap(), Some(updated));
    }

    #[test]
    fn treats_analyzer_or_source_identity_changes_as_cache_misses() {
        let directory = TestDirectory::new();
        let cache = AnalysisCache::new(directory.path(), "tempo-v1").unwrap();
        let key =
            AnalysisCacheKey::new("youtube", "abc", Some(100), Some(Duration::from_secs(180)))
                .unwrap();
        cache.store(&key, &analysis()).unwrap();

        let newer_analyzer = AnalysisCache::new(directory.path(), "tempo-v2").unwrap();
        assert_eq!(newer_analyzer.load(&key).unwrap(), None);

        let changed_length =
            AnalysisCacheKey::new("youtube", "abc", Some(101), Some(Duration::from_secs(180)))
                .unwrap();
        assert_eq!(cache.load(&changed_length).unwrap(), None);

        let changed_duration =
            AnalysisCacheKey::new("youtube", "abc", Some(100), Some(Duration::from_secs(182)))
                .unwrap();
        assert_eq!(cache.load(&changed_duration).unwrap(), None);
    }

    #[test]
    fn permanent_classical_backend_is_separate_from_neural_cache() {
        let directory = TestDirectory::new();
        let neural = AnalysisCache::new(directory.path(), "neural-v12").unwrap();
        let classical =
            AnalysisCache::new(directory.path().join("classical"), "classical-v11").unwrap();
        let key = AnalysisCacheKey::new("youtube", "abc", None, None).unwrap();

        classical.store(&key, &analysis()).unwrap();
        assert_eq!(neural.load(&key).unwrap(), None);
        assert_eq!(classical.load(&key).unwrap(), Some(analysis()));
    }

    #[test]
    fn treats_previous_marker_schema_as_a_cache_miss() {
        let directory = TestDirectory::new();
        let cache = AnalysisCache::new(directory.path(), "tempo-v1").unwrap();
        let key = AnalysisCacheKey::new("youtube", "abc", None, None).unwrap();
        let mut record = CachedAnalysis::new(&key, "tempo-v1", &analysis());
        // v9 used the former marker-boundary semantics. It must never be
        // reused after the Extended intro/outro marker update.
        record.schema_version = 9;
        let file = File::create(cache.path_for(&key)).unwrap();
        serde_json::to_writer(file, &record).unwrap();

        assert_eq!(cache.load(&key).unwrap(), None);
    }

    #[test]
    fn rejects_invalid_analysis_before_writing() {
        let directory = TestDirectory::new();
        let cache = AnalysisCache::new(directory.path(), "tempo-v1").unwrap();
        let key = AnalysisCacheKey::new("youtube", "abc", None, None).unwrap();
        let invalid = TrackAnalysis {
            duration: Duration::from_secs(1),
            audible_start: Duration::from_millis(900),
            audible_end: Duration::from_millis(800),
            intro_end: None,
            intro_confidence: 0.0,
            outro_start: None,
            outro_confidence: 0.0,
            vocal_activity: Vec::new(),
            vocal_activity_confidences: Vec::new(),
            vocal_activity_rate: 0,
            energy_profile: Vec::new(),
            energy_profile_rate: 0,
            bpm: Some(f32::NAN),
            beat_confidence: 2.0,
            first_beat: None,
            beat_markers: Vec::new(),
            beat_marker_confidences: Vec::new(),
            first_downbeat: None,
            downbeat_confidence: 2.0,
            musical_key: None,
            rms_dbfs: Some(f32::NAN),
            sample_peak_dbfs: None,
            integrated_lufs: None,
            true_peak_dbtp: None,
        };

        assert!(matches!(
            cache.store(&key, &invalid),
            Err(AnalysisCacheError::InvalidAnalysis(_))
        ));
        assert!(!cache.path_for(&key).exists());
    }

    #[test]
    fn rejects_marker_confidence_length_mismatch() {
        let directory = TestDirectory::new();
        let cache = AnalysisCache::new(directory.path(), "tempo-v1").unwrap();
        let key = AnalysisCacheKey::new("youtube", "abc", None, None).unwrap();
        let mut invalid = analysis();
        invalid.beat_marker_confidences.push(0.5);

        assert!(matches!(
            cache.store(&key, &invalid),
            Err(AnalysisCacheError::InvalidAnalysis(_))
        ));
    }

    #[test]
    fn rejects_non_finite_loudness_measurements() {
        let directory = TestDirectory::new();
        let cache = AnalysisCache::new(directory.path(), "tempo-v1").unwrap();
        let key = AnalysisCacheKey::new("youtube", "abc", None, None).unwrap();

        let mut invalid_loudness = analysis();
        invalid_loudness.integrated_lufs = Some(f32::NAN);
        assert!(matches!(
            cache.store(&key, &invalid_loudness),
            Err(AnalysisCacheError::InvalidAnalysis(_))
        ));

        let mut invalid_peak = analysis();
        invalid_peak.true_peak_dbtp = Some(f32::INFINITY);
        assert!(matches!(
            cache.store(&key, &invalid_peak),
            Err(AnalysisCacheError::InvalidAnalysis(_))
        ));
        assert!(!cache.path_for(&key).exists());
    }

    #[test]
    fn rejects_truncated_vocal_activity_profile() {
        let directory = TestDirectory::new();
        let cache = AnalysisCache::new(directory.path(), "tempo-v1").unwrap();
        let key = AnalysisCacheKey::new("youtube", "abc", None, None).unwrap();
        let mut invalid = analysis();
        invalid.vocal_activity.truncate(2);
        invalid.vocal_activity_confidences.truncate(2);

        assert!(matches!(
            cache.store(&key, &invalid),
            Err(AnalysisCacheError::InvalidAnalysis(_))
        ));
    }

    #[test]
    fn legacy_analysis_without_new_analysis_fields_still_decodes() {
        let mut value = serde_json::to_value(SerializableAnalysis::from(&analysis())).unwrap();
        let object = value.as_object_mut().unwrap();
        for field in [
            "beat_marker_confidences",
            "intro_end_micros",
            "intro_confidence",
            "outro_start_micros",
            "outro_confidence",
            "vocal_activity",
            "vocal_activity_confidences",
            "vocal_activity_rate",
            "energy_profile",
            "energy_profile_rate",
            "integrated_lufs",
            "true_peak_dbtp",
        ] {
            object.remove(field);
        }
        let serialized: SerializableAnalysis = serde_json::from_value(value).unwrap();
        let decoded = TrackAnalysis::try_from(serialized).unwrap();

        assert!(decoded.beat_marker_confidences.is_empty());
        assert_eq!(decoded.beat_markers, analysis().beat_markers);
        assert_eq!(decoded.intro_end, None);
        assert_eq!(decoded.outro_start, None);
        assert_eq!(decoded.vocal_activity_rate, 0);
        assert_eq!(decoded.energy_profile_rate, 0);
        assert_eq!(decoded.integrated_lufs, None);
        assert_eq!(decoded.true_peak_dbtp, None);
    }

    #[test]
    fn stores_long_high_tempo_marker_analysis_with_confidences() {
        let directory = TestDirectory::new();
        let cache = AnalysisCache::new(directory.path(), "tempo-v1").unwrap();
        let key = AnalysisCacheKey::new("youtube", "long", None, None).unwrap();
        let mut long = analysis();
        long.duration = Duration::from_secs(30 * 60);
        long.audible_end = long.duration;
        long.beat_markers = (0..5_400)
            .map(|index| Duration::from_millis(index * 1_000 / 3))
            .collect();
        long.beat_marker_confidences = vec![1.0; long.beat_markers.len()];
        long.vocal_activity = vec![0; 30 * 60 * 4];
        long.vocal_activity_confidences = vec![255; 30 * 60 * 4];
        long.energy_profile = vec![192; 30 * 60 * 4];

        cache.store(&key, &long).unwrap();
        assert_eq!(cache.load(&key).unwrap(), Some(long));
        assert!(cache.path_for(&key).metadata().unwrap().len() < MAX_CACHE_FILE_BYTES);
    }

    fn analysis_v2() -> TrackAnalysisV2 {
        let mut analysis = TrackAnalysisV2::unanalyzed(Duration::from_secs(10));
        analysis.provenance = AnalysisProvenance::new("automix-v2-test").unwrap();
        analysis.provenance.schema_version = Some("2".to_owned());
        analysis.provenance.rhythm = Some(ComponentProvenance {
            component: "rhythm".to_owned(),
            method: AnalysisMethod::Neural,
            model: Some(ModelIdentity::new("beat-this", "1.0").unwrap()),
            confidence: Some(Confidence::new(0.9).unwrap()),
            notes: Some("event-preserving decoder".to_owned()),
        });
        analysis.provenance.overall_confidence = Some(UnitInterval::new(0.8).unwrap());
        analysis.rhythm.beats = [0_u64, 500, 1_000]
            .into_iter()
            .map(|millis| BeatEvent::at(Duration::from_millis(millis), Confidence::ONE))
            .collect();
        analysis.rhythm.tempo_hypotheses.push(TempoHypothesis {
            bpm: 120.0,
            relative_weight: UnitInterval::ONE,
            relation: TempoRelation::Primary,
        });
        analysis
            .rhythm
            .meter_hypotheses
            .push(MeterHypothesis::new(4, 0, UnitInterval::ONE).unwrap());
        analysis.tonal.local_windows.push(
            LocalTonalWindow::new(
                Duration::from_secs(2),
                Duration::from_secs(4),
                None,
                Some(Confidence::new(0.7).unwrap()),
            )
            .unwrap(),
        );
        analysis.energy.rms_dbfs = Some(-18.0);
        analysis.energy.sample_peak_dbfs = Some(-1.0);
        analysis.energy.integrated_lufs = Some(-14.0);
        analysis.energy.true_peak_dbtp = Some(-0.5);
        analysis.structure.sections = vec![Section::new(0, 3, 0.8, [SectionLabel::Verse])];
        analysis
            .cues
            .push(DjCue::new(1, UnitInterval::clamped(0.5)));
        analysis
    }

    #[test]
    fn v2_cache_roundtrip_preserves_event_timeline_and_shape() {
        let directory = TestDirectory::new();
        let cache = AnalysisCache::new(directory.path(), "automix-v2-test").unwrap();
        let key = AnalysisCacheKey::new(
            "youtube",
            "v2-track",
            Some(1234),
            Some(Duration::from_secs(10)),
        )
        .unwrap();
        let expected = analysis_v2();

        cache.store_v2(&key, &expected).unwrap();
        let restored = cache.load_v2(&key).unwrap().expect("V2 cache hit");
        assert_eq!(restored, expected);
        assert_eq!(restored.rhythm.beats.len(), 3);
        assert_eq!(restored.rhythm.beats[1].time, Duration::from_millis(500));
        assert_eq!(restored.structure.sections[0].end_beat, 3);
        assert_eq!(restored.cues[0].beat_index, 1);
    }

    #[test]
    fn v2_cache_roundtrip_preserves_rekordbox_hot_cue_provenance() {
        let directory = TestDirectory::new();
        let cache = AnalysisCache::new(directory.path(), "automix-v2-test").unwrap();
        let key = AnalysisCacheKey::new("youtube", "v2-hot-cue", None, None).unwrap();
        let mut expected = analysis_v2();
        let mut hot_cue = DjCue::new(2, UnitInterval::ONE);
        hot_cue.provenance = CueProvenance::Imported(HumanCueSource::RekordboxHotCue);
        expected.cues.push(hot_cue);

        cache.store_v2(&key, &expected).unwrap();
        let restored = cache.load_v2(&key).unwrap().expect("V2 cache hit");
        assert_eq!(restored.cues[1].provenance, hot_cue.provenance);
    }

    #[test]
    fn v2_cache_namespace_and_schema_mismatch_are_cache_misses() {
        let directory = TestDirectory::new();
        let cache = AnalysisCache::new(directory.path(), "automix-v2-test").unwrap();
        let key = AnalysisCacheKey::new("youtube", "v2-schema", None, None).unwrap();
        cache.store_v2(&key, &analysis_v2()).unwrap();

        // A V1 lookup must never consume the V2 binary record.
        assert_eq!(cache.load(&key).unwrap(), None);

        let path = cache.path_for_v2(&key);
        let mut bytes = fs::read(&path).unwrap();
        assert_eq!(&bytes[..4], b"WAM2");
        bytes[4..8].copy_from_slice(&(ANALYSIS_CACHE_V2_SCHEMA_VERSION - 1).to_le_bytes());
        fs::write(&path, bytes).unwrap();
        assert_eq!(cache.load_v2(&key).unwrap(), None);
    }

    #[test]
    fn malformed_v2_cache_fails_closed_without_panicking() {
        let directory = TestDirectory::new();
        let cache = AnalysisCache::new(directory.path(), "automix-v2-test").unwrap();
        let key = AnalysisCacheKey::new("youtube", "v2-malformed", None, None).unwrap();
        fs::write(cache.path_for_v2(&key), b"WAM2\x0c\0\0\0").unwrap();

        let result = std::panic::catch_unwind(|| cache.load_v2(&key));
        assert!(result.is_ok());
        assert!(matches!(
            result.unwrap(),
            Err(AnalysisCacheError::DecodeV2 { .. })
        ));
    }

    #[test]
    fn v2_cache_rejects_profile_shape_mismatch_before_install() {
        let directory = TestDirectory::new();
        let cache = AnalysisCache::new(directory.path(), "automix-v2-test").unwrap();
        let key = AnalysisCacheKey::new("youtube", "v2-shape", None, None).unwrap();
        let mut invalid = analysis_v2();
        invalid.vocal.rate_hz = 4;
        invalid.vocal.activity = vec![UnitInterval::ONE];
        invalid.vocal.confidences = Vec::new();

        assert!(matches!(
            cache.store_v2(&key, &invalid),
            Err(AnalysisCacheError::InvalidAnalysis(_))
        ));
        assert!(!cache.path_for_v2(&key).exists());
    }

    #[test]
    fn v2_cache_rejects_local_tonal_window_outside_duration() {
        let directory = TestDirectory::new();
        let cache = AnalysisCache::new(directory.path(), "automix-v2-test").unwrap();
        let key = AnalysisCacheKey::new("youtube", "v2-local-window", None, None).unwrap();
        let mut invalid = analysis_v2();
        invalid.tonal.local_windows = vec![
            LocalTonalWindow::new(Duration::from_secs(9), Duration::from_secs(11), None, None)
                .unwrap(),
        ];

        assert!(matches!(
            cache.store_v2(&key, &invalid),
            Err(AnalysisCacheError::InvalidAnalysis(_))
        ));
        assert!(!cache.path_for_v2(&key).exists());
    }

    #[test]
    fn v2_cache_rejects_encoded_records_over_size_limit_before_install() {
        let directory = TestDirectory::new();
        let cache = AnalysisCache::new(directory.path(), "automix-v2-test").unwrap();
        let key = AnalysisCacheKey::new("youtube", "v2-size", None, None).unwrap();
        let mut oversized = analysis_v2();
        oversized.provenance.analyzer =
            "x".repeat(usize::try_from(ANALYSIS_CACHE_V2_MAX_FILE_BYTES).unwrap() + 1);

        assert!(matches!(
            cache.store_v2(&key, &oversized),
            Err(AnalysisCacheError::Oversized { .. })
        ));
        assert!(!cache.path_for_v2(&key).exists());
    }

    #[test]
    fn v2_cache_30_minute_180_bpm_events_and_profiles_fit_one_mib() {
        let directory = TestDirectory::new();
        let cache = AnalysisCache::new(directory.path(), "automix-v2-test").unwrap();
        let key = AnalysisCacheKey::new("youtube", "v2-long", None, None).unwrap();
        let duration = Duration::from_secs(30 * 60);
        let mut analysis = TrackAnalysisV2::unanalyzed(duration);
        analysis.provenance = AnalysisProvenance::new("automix-v2-test").unwrap();
        // 180 BPM over 30 minutes is 5,400 beats.  Keep the millisecond
        // timeline strictly increasing while retaining the fractional beat
        // period through alternating 333/334 ms intervals.
        analysis.rhythm.beats = (0..5_400)
            .map(|index| BeatEvent::at(Duration::from_millis((index * 1_000) / 3), Confidence::ONE))
            .collect();
        analysis.rhythm.tempo_hypotheses.push(TempoHypothesis {
            bpm: 180.0,
            relative_weight: UnitInterval::ONE,
            relation: TempoRelation::Primary,
        });
        analysis.vocal.activity = vec![UnitInterval::ZERO; 30 * 60 * 4];
        analysis.vocal.confidences = vec![Confidence::ONE; 30 * 60 * 4];
        analysis.vocal.rate_hz = 4;
        analysis.energy.profile = vec![UnitInterval::ONE; 30 * 60 * 4];
        analysis.energy.confidences = vec![Confidence::ONE; 30 * 60 * 4];
        analysis.energy.rate_hz = 4;

        cache.store_v2(&key, &analysis).unwrap();
        let size = cache.path_for_v2(&key).metadata().unwrap().len();
        assert!(
            size < ANALYSIS_CACHE_V2_MAX_FILE_BYTES,
            "encoded V2 size={size}"
        );
        let restored = cache.load_v2(&key).unwrap().unwrap();
        assert_eq!(restored.rhythm.beats.len(), 5_400);
        assert_eq!(
            restored.rhythm.beats[5_399].time,
            analysis.rhythm.beats[5_399].time
        );
        assert_eq!(restored.vocal.activity.len(), 7_200);
        assert_eq!(restored.energy.profile.len(), 7_200);
    }

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let sequence = NEXT_TEMP_FILE_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "wotoha-analysis-cache-test-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}
