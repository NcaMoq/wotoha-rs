use std::{
    fs::{self, File, OpenOptions},
    io::{BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use wotoha_core::{PreparedSource, TrackRequest, automix::TrackAnalysis};

pub const ANALYSIS_CACHE_SCHEMA_VERSION: u32 = 1;
const MAX_CACHE_FILE_BYTES: u64 = 64 * 1024;
const SOURCE_DURATION_TOLERANCE_MICROS: u64 = 1_000_000;

static NEXT_TEMP_FILE_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Eq, PartialEq)]
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

    fn path_for(&self, key: &AnalysisCacheKey) -> PathBuf {
        self.root.join(format!("{}.json", key.digest()))
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
    #[error("failed to encode analysis cache record: {0}")]
    Encode(#[source] serde_json::Error),
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
    bpm: Option<f32>,
    beat_confidence: f32,
}

impl From<&TrackAnalysis> for SerializableAnalysis {
    fn from(value: &TrackAnalysis) -> Self {
        Self {
            duration_micros: duration_to_micros(value.duration),
            audible_start_micros: duration_to_micros(value.audible_start),
            audible_end_micros: duration_to_micros(value.audible_end),
            bpm: value.bpm,
            beat_confidence: value.beat_confidence,
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
            bpm: value.bpm,
            beat_confidence: value.beat_confidence,
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
    Ok(())
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

    fn analysis() -> TrackAnalysis {
        TrackAnalysis {
            duration: Duration::from_secs(180),
            audible_start: Duration::from_millis(750),
            audible_end: Duration::from_millis(179_200),
            bpm: Some(124.5),
            beat_confidence: 0.91,
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

        let filename = cache.path_for(&key).file_name().unwrap().to_string_lossy();
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
    fn rejects_invalid_analysis_before_writing() {
        let directory = TestDirectory::new();
        let cache = AnalysisCache::new(directory.path(), "tempo-v1").unwrap();
        let key = AnalysisCacheKey::new("youtube", "abc", None, None).unwrap();
        let invalid = TrackAnalysis {
            duration: Duration::from_secs(1),
            audible_start: Duration::from_millis(900),
            audible_end: Duration::from_millis(800),
            bpm: Some(f32::NAN),
            beat_confidence: 2.0,
        };

        assert!(matches!(
            cache.store(&key, &invalid),
            Err(AnalysisCacheError::InvalidAnalysis(_))
        ));
        assert!(!cache.path_for(&key).exists());
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
