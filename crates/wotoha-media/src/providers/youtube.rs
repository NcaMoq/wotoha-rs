use std::{
    collections::{HashMap, HashSet},
    env,
    fmt::Write as FmtWrite,
    fs,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc, Mutex as StdMutex, OnceLock, RwLock as StdRwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use async_trait::async_trait;
use dashmap::DashMap;
use regex::Regex;
use reqwest::{
    Client, StatusCode, Url,
    header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, RANGE},
    redirect::Policy,
};
use rusty_ytdl::{
    Video, VideoFormat, VideoOptions, VideoQuality, VideoSearchOptions, choose_format,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{Mutex as AsyncMutex, OwnedMutexGuard, RwLock as AsyncRwLock, Semaphore},
};
use wotoha_core::{PreparedHeader, PreparedRangeMode, PreparedSource, TrackMetadata, TrackRequest};

use crate::{ResolveError, provider::MediaProvider};

use super::{
    youtube_pot::{self, PoToken, PoTokenClient, PoTokenContext},
    youtube_ytdlp,
};

const YOUTUBE_RANGE_CHUNK_SIZE: u64 = 11_862_014;
const YOUTUBE_NATIVE_FAST_PATH_TIMEOUT: Duration = Duration::from_secs(2);
const YOUTUBE_JS_SLOW_PATH_TIMEOUT: Duration = Duration::from_secs(15);
const YOUTUBE_STREAM_VALIDATION_TIMEOUT: Duration = Duration::from_secs(4);
const YOUTUBE_STREAM_VALIDATION_RANGE: &str = "bytes=0-1023";
const YOUTUBE_VISITOR_SESSION_TTL: Duration = Duration::from_secs(30 * 60);
const YOUTUBE_PO_TOKEN_FAILURE_BACKOFF: Duration = Duration::from_secs(30);
const YOUTUBE_PO_TOKEN_MAX_CONCURRENCY: usize = 2;
const YOUTUBE_PO_TOKEN_FAST_PATH_TIMEOUT: Duration = Duration::from_secs(15);
const YOUTUBE_PLAYER_SCRIPT_MAX_BYTES: usize = 8 * 1024 * 1024;
const YOUTUBE_PLAYER_SCRIPT_TTL: Duration = Duration::from_secs(6 * 60 * 60);
const YOUTUBE_JS_WORKER_PROTOCOL_VERSION: u32 = 1;
const YOUTUBE_JS_WORKER_SESSION_LIMIT: usize = 4;
const YOUTUBE_JS_WORKER_REQUEST_MAX_BYTES: usize = 12 * 1024 * 1024;
const YOUTUBE_JS_WORKER_RESPONSE_MAX_BYTES: usize = 2 * 1024 * 1024;
const YOUTUBE_JS_WORKER_QUEUE_TIMEOUT: Duration = Duration::from_secs(12);
const YOUTUBE_JS_WORKER_EXECUTION_TIMEOUT: Duration = Duration::from_secs(12);
const YOUTUBE_JS_WORKER_CANDIDATE_TIMEOUT: Duration = Duration::from_secs(6);
const YOUTUBE_JS_WORKER_FAILURE_BACKOFF: Duration = Duration::from_secs(30);
const YOUTUBE_JS_WORKER_CANDIDATE_REJECTION_TTL: Duration = Duration::from_secs(5 * 60);
const YOUTUBE_CHALLENGE_JOB_LIMIT: usize = 64;
const YOUTUBE_CHALLENGE_VALUE_MAX_BYTES: usize = 16 * 1024;

static YOUTUBE_VISITOR_SESSION: OnceLock<AsyncRwLock<Option<CachedVisitorSession>>> =
    OnceLock::new();
static YOUTUBE_NATIVE_CLIENTS: OnceLock<StdRwLock<CachedNativeClients>> = OnceLock::new();
static YOUTUBE_PO_TOKEN_CACHE: OnceLock<DashMap<String, PoToken>> = OnceLock::new();
static YOUTUBE_PO_TOKEN_PROVIDER_SLOTS: OnceLock<Semaphore> = OnceLock::new();
static YOUTUBE_PO_TOKEN_PROVIDER_BACKOFF: OnceLock<StdMutex<Option<Instant>>> = OnceLock::new();
static YOUTUBE_PLAYER_SCRIPT: OnceLock<AsyncMutex<Option<CachedPlayerScript>>> = OnceLock::new();
static YOUTUBE_PLAYER_CLIENT: OnceLock<Result<Client, String>> = OnceLock::new();
static YOUTUBE_JS_WORKER: OnceLock<AsyncMutex<JsWorkerSupervisor>> = OnceLock::new();
static YOUTUBE_JS_WORKER_VERIFIED_DIGESTS: OnceLock<StdMutex<HashSet<String>>> = OnceLock::new();

#[derive(Clone, Debug)]
struct CachedVisitorSession {
    visitor_data: String,
    signature_timestamp: u64,
    player_url: Option<String>,
    cached_at: Instant,
}

#[derive(Clone, Debug)]
struct CachedPlayerScript {
    url: String,
    source: Arc<str>,
    cached_at: Instant,
}

#[derive(Clone, Debug, Serialize)]
struct ChallengeInput {
    signature: Option<String>,
    n: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
struct ChallengeOutput {
    signature: Option<String>,
    n: Option<String>,
}

#[derive(Serialize)]
struct JsWorkerRequest<'a> {
    protocol_version: u32,
    request_id: u64,
    player_key: &'a str,
    player_source: Option<&'a str>,
    inputs: &'a [ChallengeInput],
    per_input_results: bool,
}

#[derive(Deserialize)]
struct JsWorkerResponse {
    protocol_version: u32,
    request_id: Option<u64>,
    outputs: Option<Vec<ChallengeOutput>>,
    results: Option<Vec<JsWorkerChallengeResult>>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct JsWorkerChallengeResult {
    output: Option<ChallengeOutput>,
    error: Option<String>,
}

#[derive(Default)]
struct JsWorkerSupervisor {
    current: Option<Arc<JsWorkerLane>>,
    candidate: Option<Arc<JsWorkerLane>>,
    failure_until: HashMap<WorkerPlayerKey, Instant>,
    rejected_candidates: HashMap<WorkerPlayerKey, Instant>,
}

struct JsWorkerLane {
    executable: JsWorkerExecutable,
    process: Arc<AsyncMutex<Option<JsWorkerProcess>>>,
}

struct JsWorkerProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    loaded_players: HashSet<String>,
    next_request_id: u64,
}

struct JsWorkerProcessLease {
    guard: OwnedMutexGuard<Option<JsWorkerProcess>>,
    process: Option<JsWorkerProcess>,
}

#[derive(Clone)]
struct JsWorkerExecutable {
    path: PathBuf,
    app_worker_mode: bool,
    identity: String,
}

#[derive(Clone)]
struct JsWorkerCandidate {
    executable: JsWorkerExecutable,
    release_id: String,
    ack_path: PathBuf,
}

struct JsWorkerSelection {
    current: JsWorkerExecutable,
    candidate: Option<JsWorkerCandidate>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct WorkerPlayerKey {
    executable_identity: String,
    player_key: String,
}

#[derive(Clone)]
struct JsWorkerCandidateProof {
    release_id: String,
    ack_path: PathBuf,
    executable_identity: String,
    baseline_current_identity: String,
    player_key: String,
}

#[derive(Clone)]
struct JsWorkerCandidateRoute {
    lane: Arc<JsWorkerLane>,
    proof: JsWorkerCandidateProof,
}

struct JsWorkerRoutes {
    current: Arc<JsWorkerLane>,
    candidate: Option<JsWorkerCandidateRoute>,
}

struct JsWorkerCandidateBatch {
    outputs: Vec<Result<ChallengeOutput, String>>,
    proof: JsWorkerCandidateProof,
}

struct SolvedNChallenge {
    stream_url: String,
    candidate_proof: Option<JsWorkerCandidateProof>,
}

struct SolvedCipherPlayerResponse {
    response: AndroidPlayerResponse,
    candidate_proof: Option<JsWorkerCandidateProof>,
}

#[derive(Clone, Copy)]
enum CipherWorkerChoice {
    Current,
    Candidate,
}

struct SolvedCipherFormats {
    solved: usize,
    candidate_proof: Option<JsWorkerCandidateProof>,
}

#[derive(Clone, Debug)]
struct CachedNativeClients {
    path: PathBuf,
    modified_at: Option<SystemTime>,
    profiles: Arc<[NativeClientProfile]>,
}

#[derive(Clone, Debug, Deserialize)]
struct NativeClientProfile {
    id: String,
    client_name: String,
    client_version: String,
    client_number: String,
    user_agent: String,
    os_name: String,
    os_version: String,
    device_make: Option<String>,
    device_model: Option<String>,
    android_sdk_version: Option<u64>,
}

// These are ordered by expected GVS reliability. Every returned URL is verified before use,
// so a rollout that breaks one client automatically falls through to the next strategy.
fn default_native_client_profiles() -> Vec<NativeClientProfile> {
    vec![
        NativeClientProfile {
            id: "visionos".to_owned(),
            client_name: "VISIONOS".to_owned(),
            client_version: "1.02".to_owned(),
            client_number: "101".to_owned(),
            user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 15_7_3) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15".to_owned(),
            os_name: "visionOS".to_owned(),
            os_version: "26.5.23O471".to_owned(),
            device_make: Some("Apple".to_owned()),
            device_model: Some("RealityDevice17,1".to_owned()),
            android_sdk_version: None,
        },
        NativeClientProfile {
            id: "android_vr".to_owned(),
            client_name: "ANDROID_VR".to_owned(),
            client_version: "1.65.10".to_owned(),
            client_number: "28".to_owned(),
            user_agent: "com.google.android.apps.youtube.vr.oculus/1.65.10 (Linux; U; Android 12L; eureka-user Build/SQ3A.220605.009.A1) gzip".to_owned(),
            os_name: "Android".to_owned(),
            os_version: "12L".to_owned(),
            device_make: Some("Oculus".to_owned()),
            device_model: Some("Quest 3".to_owned()),
            android_sdk_version: Some(32),
        },
        NativeClientProfile {
            id: "android".to_owned(),
            client_name: "ANDROID".to_owned(),
            client_version: "21.26.364".to_owned(),
            client_number: "3".to_owned(),
            user_agent: "com.google.android.youtube/21.26.364 (Linux; U; Android 11) gzip".to_owned(),
            os_name: "Android".to_owned(),
            os_version: "11".to_owned(),
            device_make: None,
            device_model: None,
            android_sdk_version: Some(30),
        },
    ]
}

fn native_client_profiles() -> Arc<[NativeClientProfile]> {
    let path = env::var_os("WOTOHA_YOUTUBE_CLIENTS_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/etc/wotoha/youtube-clients.json"));
    let modified_at = fs::metadata(&path)
        .ok()
        .and_then(|metadata| metadata.modified().ok());
    let cache = YOUTUBE_NATIVE_CLIENTS.get_or_init(|| {
        StdRwLock::new(CachedNativeClients {
            path: PathBuf::new(),
            modified_at: None,
            profiles: default_native_client_profiles().into(),
        })
    });
    {
        let cached = cache.read().expect("YouTube client cache read lock");
        if cached.path == path && cached.modified_at == modified_at {
            return cached.profiles.clone();
        }
    }

    let profiles: Arc<[NativeClientProfile]> = load_native_client_profiles(&path).into();
    let mut cached = cache.write().expect("YouTube client cache write lock");
    if cached.path != path || cached.modified_at != modified_at {
        cached.path = path;
        cached.modified_at = modified_at;
        cached.profiles = profiles;
    }
    cached.profiles.clone()
}

fn visitor_native_client_profile() -> NativeClientProfile {
    native_client_profiles()
        .iter()
        .find(|profile| profile.id == "android_vr")
        .cloned()
        .unwrap_or_else(|| native_client_profiles()[0].clone())
}

fn load_native_client_profiles(path: &PathBuf) -> Vec<NativeClientProfile> {
    match fs::read_to_string(path) {
        Ok(json) => match serde_json::from_str::<Vec<NativeClientProfile>>(&json) {
            Ok(profiles) if validate_native_client_profiles(&profiles) => {
                tracing::info!(
                    path = %path.display(),
                    profiles = profiles.len(),
                    "loaded external YouTube native client profiles"
                );
                profiles
            }
            Ok(_) => {
                tracing::warn!(
                    path = %path.display(),
                    "ignored invalid YouTube native client profile file"
                );
                default_native_client_profiles()
            }
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %error,
                    "failed to parse YouTube native client profile file"
                );
                default_native_client_profiles()
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            default_native_client_profiles()
        }
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                error = %error,
                "failed to read YouTube native client profile file"
            );
            default_native_client_profiles()
        }
    }
}

fn validate_native_client_profiles(profiles: &[NativeClientProfile]) -> bool {
    let mut profile_ids = HashSet::with_capacity(profiles.len());
    !profiles.is_empty()
        && profiles.len() <= 8
        && profiles.iter().all(|profile| {
            profile_ids.insert(profile.id.as_str())
                && !profile.id.is_empty()
                && profile
                    .id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                && !profile.client_name.is_empty()
                && profile.client_name.len() <= 64
                && !profile.client_version.is_empty()
                && profile.client_version.len() <= 64
                && profile.client_number.parse::<u16>().is_ok()
                && !profile.user_agent.is_empty()
                && profile.user_agent.len() <= 512
                && !profile.os_name.is_empty()
                && profile.os_name.len() <= 64
                && !profile.os_version.is_empty()
                && profile.os_version.len() <= 64
        })
}

async fn po_token(
    profile: &NativeClientProfile,
    context: PoTokenContext,
    video_id: &str,
    visitor_data: Option<&str>,
    bypass_cache: bool,
) -> Option<(PoToken, bool)> {
    if !youtube_pot::is_configured() {
        return None;
    }
    let context_name = match context {
        PoTokenContext::Player => "player",
        PoTokenContext::Gvs => "gvs",
    };
    let cache_key = po_token_cache_key(profile, context_name, video_id, visitor_data);
    let cache = YOUTUBE_PO_TOKEN_CACHE.get_or_init(DashMap::new);
    if !bypass_cache && let Some(token) = cached_po_token(cache, &cache_key) {
        return Some((token, true));
    }
    if bypass_cache {
        cache.remove(&cache_key);
    }
    if po_token_provider_is_backing_off() {
        return None;
    }

    let slots = YOUTUBE_PO_TOKEN_PROVIDER_SLOTS
        .get_or_init(|| Semaphore::new(YOUTUBE_PO_TOKEN_MAX_CONCURRENCY));
    let Ok(_permit) = slots.acquire().await else {
        return None;
    };
    if !bypass_cache && let Some(token) = cached_po_token(cache, &cache_key) {
        return Some((token, true));
    }
    if po_token_provider_is_backing_off() {
        return None;
    }

    let client = PoTokenClient {
        profile_id: profile.id.as_str(),
        client_name: profile.client_name.as_str(),
        client_version: profile.client_version.as_str(),
        client_number: profile.client_number.as_str(),
        user_agent: profile.user_agent.as_str(),
        os_name: profile.os_name.as_str(),
        os_version: profile.os_version.as_str(),
        device_make: profile.device_make.as_deref(),
        device_model: profile.device_model.as_deref(),
        android_sdk_version: profile.android_sdk_version,
    };
    match youtube_pot::request_token(client, context, video_id, visitor_data).await {
        Ok(Some(token)) => {
            if token.expires_at > Instant::now() + Duration::from_secs(5) {
                cache.insert(cache_key.clone(), token.clone());
            }
            if cache.len() > 4_096 {
                let now = Instant::now();
                cache.retain(|_, token| token.expires_at > now);
                let overflow = cache.len().saturating_sub(4_096);
                let keys = cache
                    .iter()
                    .filter(|entry| entry.key().as_str() != cache_key)
                    .take(overflow)
                    .map(|entry| entry.key().clone())
                    .collect::<Vec<_>>();
                for key in keys {
                    cache.remove(&key);
                }
            }
            Some((token, false))
        }
        Ok(None) => None,
        Err(error) => {
            *YOUTUBE_PO_TOKEN_PROVIDER_BACKOFF
                .get_or_init(|| StdMutex::new(None))
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                Some(Instant::now() + YOUTUBE_PO_TOKEN_FAILURE_BACKOFF);
            tracing::warn!(
                extractor = "native",
                strategy = profile.id.as_str(),
                context = context_name,
                error = %error,
                "YouTube PO Token provider failed"
            );
            None
        }
    }
}

fn po_token_cache_key(
    profile: &NativeClientProfile,
    context_name: &str,
    video_id: &str,
    visitor_data: Option<&str>,
) -> String {
    serde_json::to_string(&(
        profile.id.as_str(),
        profile.client_name.as_str(),
        profile.client_version.as_str(),
        profile.client_number.as_str(),
        profile.user_agent.as_str(),
        context_name,
        video_id,
        visitor_data.unwrap_or_default(),
    ))
    .expect("PO Token cache key fields should serialize")
}

fn invalidate_po_token(
    profile: &NativeClientProfile,
    context: PoTokenContext,
    video_id: &str,
    visitor_data: Option<&str>,
) {
    let context_name = match context {
        PoTokenContext::Player => "player",
        PoTokenContext::Gvs => "gvs",
    };
    if let Some(cache) = YOUTUBE_PO_TOKEN_CACHE.get() {
        cache.remove(&po_token_cache_key(
            profile,
            context_name,
            video_id,
            visitor_data,
        ));
    }
}

fn cached_po_token(cache: &DashMap<String, PoToken>, cache_key: &str) -> Option<PoToken> {
    if let Some(cached) = cache.get(cache_key)
        && cached.expires_at > Instant::now() + Duration::from_secs(5)
    {
        return Some(cached.clone());
    }
    cache.remove(cache_key);
    None
}

fn po_token_provider_is_backing_off() -> bool {
    let mut backoff = YOUTUBE_PO_TOKEN_PROVIDER_BACKOFF
        .get_or_init(|| StdMutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if backoff.is_some_and(|until| until > Instant::now()) {
        true
    } else {
        *backoff = None;
        false
    }
}

#[derive(Clone, Debug, Default)]
pub struct YouTubeProvider;

#[async_trait]
impl MediaProvider for YouTubeProvider {
    fn id(&self) -> &'static str {
        "youtube"
    }

    fn supports(&self, raw_url: &str) -> bool {
        let Ok(url) = Url::parse(raw_url) else {
            return false;
        };

        matches!(
            url.host_str().map(|host| host.to_ascii_lowercase()),
            Some(host)
                if matches!(
                    host.as_str(),
                    "youtube.com"
                        | "www.youtube.com"
                        | "m.youtube.com"
                        | "music.youtube.com"
                        | "youtu.be"
                )
        )
    }

    async fn probe(
        &self,
        raw_url: &str,
        probe_client: &Client,
    ) -> Result<TrackRequest, ResolveError> {
        match fetch_android_vr_track_request_fast(raw_url, probe_client).await {
            Ok(Some(request)) => return Ok(request),
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "YouTube native fast path failed"
                );
            }
        }

        match youtube_ytdlp::probe(raw_url).await {
            Ok(request) => match validate_prepared_source(probe_client, &request.prepared).await {
                Ok(()) => {
                    tracing::info!(
                        extractor = "yt-dlp",
                        video_key = %request.canonical_key,
                        "YouTube extraction selected verified fallback"
                    );
                    return Ok(request);
                }
                Err(error) => {
                    tracing::warn!(
                        extractor = "yt-dlp",
                        error = %error,
                        "YouTube fallback returned an unplayable stream"
                    );
                }
            },
            Err(error) => {
                tracing::warn!(
                    extractor = "yt-dlp",
                    error = %error,
                    "YouTube maintained fallback extraction failed"
                );
            }
        }

        let options = youtube_options(probe_client.clone());
        let video =
            Video::new_with_options(raw_url, options.clone()).map_err(ResolveError::YouTube)?;
        let info = video.get_info().await.map_err(ResolveError::YouTube)?;
        let details = info.video_details;
        let canonical_url = format!("https://www.youtube.com/watch?v={}", details.video_id);
        let android_format = match fetch_android_vr_audio_format(
            probe_client,
            &canonical_url,
            details.video_id.as_str(),
        )
        .await
        {
            Ok(format) => format,
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "YouTube Android VR extraction failed; falling back to rusty_ytdl formats"
                );
                None
            }
        };
        let format = android_format
            .or_else(|| {
                choose_playable_format(&info.formats, &info.hls_manifest_url, &options).ok()
            })
            .ok_or_else(|| {
                ResolveError::Parse("YouTube did not expose a playable stream URL".to_owned())
            })?;

        let expires_at_unix = earliest_expiry(
            format_url_expiry(format.stream_url.as_ref()),
            format.po_token_expires_at_unix,
        );
        let content_length = format.content_length.as_deref().and_then(|value| {
            value
                .parse::<u64>()
                .ok()
                .or_else(|| parse_content_length_from_url(format.stream_url.as_ref()))
        });

        let prepared = prepared_source_from_format(
            format,
            details.is_live_content,
            content_length,
            expires_at_unix,
        );
        validate_prepared_source(probe_client, &prepared)
            .await
            .map_err(|error| {
                ResolveError::Parse(format!(
                    "YouTube extractors did not expose a verified stream: {error}"
                ))
            })?;

        Ok(TrackRequest::new(
            self.id(),
            format!("youtube:video:{}", details.video_id),
            raw_url.to_owned(),
            canonical_url.clone(),
            canonical_url.clone(),
            prepared,
            TrackMetadata::new(
                details.title,
                details
                    .author
                    .map(|author| author.name)
                    .filter(|value| !value.is_empty())
                    .unwrap_or(details.owner_channel_name),
                canonical_url,
                pick_thumbnail(&details.thumbnails),
                parse_duration(&details.length_seconds, details.is_live_content),
            ),
        ))
    }

    async fn refresh_playback(
        &self,
        request: &TrackRequest,
        probe_client: &Client,
    ) -> Result<Option<TrackRequest>, ResolveError> {
        let Some(video_id) = youtube_video_id(request) else {
            return Ok(None);
        };
        let format = match fetch_android_vr_audio_format(
            probe_client,
            request.canonical_url.as_ref(),
            &video_id,
        )
        .await
        {
            Ok(Some(format)) => format,
            Ok(None) => return refresh_with_ytdlp(request, probe_client).await,
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "YouTube native playback refresh failed"
                );
                return refresh_with_ytdlp(request, probe_client).await;
            }
        };

        Ok(Some(track_request_with_format(request, format)))
    }
}

async fn refresh_with_ytdlp(
    request: &TrackRequest,
    probe_client: &Client,
) -> Result<Option<TrackRequest>, ResolveError> {
    match youtube_ytdlp::refresh(request).await {
        Ok(refreshed) => match validate_prepared_source(probe_client, &refreshed.prepared).await {
            Ok(()) => Ok(Some(refreshed)),
            Err(error) => {
                tracing::warn!(
                    extractor = "yt-dlp",
                    error = %error,
                    "YouTube fallback refresh returned an unplayable stream"
                );
                Ok(None)
            }
        },
        Err(error) => {
            tracing::warn!(
                extractor = "yt-dlp",
                error = %error,
                "YouTube fallback refresh failed"
            );
            Ok(None)
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ChosenFormat {
    stream_url: String,
    content_length: Option<String>,
    is_hls: bool,
    headers: Vec<PreparedHeader>,
    range_chunk_size: Option<u64>,
    po_token_expires_at_unix: Option<u64>,
}

#[derive(Debug, thiserror::Error)]
enum StreamValidationError {
    #[error("{0}")]
    InvalidRequest(String),
    #[error("stream request failed: {0}")]
    Request(reqwest::Error),
    #[error("stream returned {0}")]
    HttpStatus(StatusCode),
    #[error("stream returned only {0} byte(s)")]
    InsufficientBody(usize),
    #[error("HLS stream did not return an M3U8 playlist")]
    InvalidHlsBody,
}

impl StreamValidationError {
    fn may_need_po_token(&self) -> bool {
        matches!(
            self,
            Self::HttpStatus(StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
        )
    }
}

fn track_request_with_format(request: &TrackRequest, format: ChosenFormat) -> TrackRequest {
    let expires_at_unix = earliest_expiry(
        format_url_expiry(format.stream_url.as_ref()),
        format.po_token_expires_at_unix,
    );
    let content_length = format.content_length.as_deref().and_then(|value| {
        value
            .parse::<u64>()
            .ok()
            .or_else(|| parse_content_length_from_url(format.stream_url.as_ref()))
    });

    let prepared = prepared_source_from_format(
        format,
        matches!(request.prepared, PreparedSource::Hls { .. }),
        content_length,
        expires_at_unix,
    );

    TrackRequest::new(
        request.provider_id.clone(),
        request.canonical_key.clone(),
        request.requested_url.clone(),
        request.canonical_url.clone(),
        request.source_url.clone(),
        prepared,
        request.metadata.clone(),
    )
}

fn prepared_source_from_format(
    format: ChosenFormat,
    prefer_hls: bool,
    content_length: Option<u64>,
    expires_at_unix: Option<u64>,
) -> PreparedSource {
    if format.is_hls || prefer_hls || looks_like_hls(format.stream_url.as_ref()) {
        PreparedSource::hls(format.stream_url, format.headers, expires_at_unix)
    } else {
        PreparedSource::http_with_range_mode(
            format.stream_url,
            format.headers,
            content_length,
            format.range_chunk_size,
            PreparedRangeMode::QueryParam,
            expires_at_unix,
        )
    }
}

async fn validate_chosen_format(
    probe_client: &Client,
    format: &ChosenFormat,
) -> Result<(), StreamValidationError> {
    let content_length = format.content_length.as_deref().and_then(|value| {
        value
            .parse::<u64>()
            .ok()
            .or_else(|| parse_content_length_from_url(format.stream_url.as_ref()))
    });
    let prepared = prepared_source_from_format(
        format.clone(),
        false,
        content_length,
        earliest_expiry(
            format_url_expiry(format.stream_url.as_ref()),
            format.po_token_expires_at_unix,
        ),
    );
    validate_prepared_source(probe_client, &prepared).await
}

async fn validate_native_format_with_pot(
    probe_client: &Client,
    format: &mut ChosenFormat,
    profile: &NativeClientProfile,
    video_id: &str,
    visitor_data: Option<&str>,
    player_url: Option<&str>,
    challenge_detected: Option<&AtomicBool>,
    allow_n_candidate_ack: bool,
) -> Result<(), String> {
    let original_url = format.stream_url.clone();
    match solve_url_n_challenge(probe_client, &original_url, player_url, challenge_detected).await {
        Ok(Some(solved)) => {
            format.stream_url = solved.stream_url;
            if validate_chosen_format(probe_client, format).await.is_ok() {
                if allow_n_candidate_ack {
                    if let Some(proof) = solved.candidate_proof.as_ref() {
                        acknowledge_validated_js_worker_candidate(proof);
                    } else if let Some(player_url) = player_url {
                        spawn_n_candidate_format_validation(
                            probe_client.clone(),
                            original_url.clone(),
                            player_url.to_owned(),
                            format.clone(),
                        );
                    }
                }
                tracing::info!(
                    extractor = "native_js",
                    "YouTube selected stream after solving the n challenge"
                );
                return Ok(());
            }
            if allow_n_candidate_ack && let Some(proof) = solved.candidate_proof.as_ref() {
                reject_js_worker_candidate(proof).await;
            }
            if solved.candidate_proof.is_none()
                && let Ok(Some(candidate)) =
                    solve_url_n_challenge_candidate(&original_url, player_url).await
            {
                format.stream_url = candidate.stream_url;
                if validate_chosen_format(probe_client, format).await.is_ok() {
                    if allow_n_candidate_ack && let Some(proof) = candidate.candidate_proof.as_ref()
                    {
                        acknowledge_validated_js_worker_candidate(proof);
                    }
                    tracing::info!(
                        extractor = "native_js",
                        "YouTube selected stream from the candidate JavaScript worker"
                    );
                    return Ok(());
                }
                if allow_n_candidate_ack && let Some(proof) = candidate.candidate_proof.as_ref() {
                    reject_js_worker_candidate(proof).await;
                }
            }
            format.stream_url = original_url;
        }
        Ok(None) => {}
        Err(error) => {
            tracing::debug!(
                extractor = "native_js",
                error = %error,
                "YouTube n challenge solve was unavailable"
            );
        }
    }
    validate_native_format_with_pot_only(probe_client, format, profile, video_id, visitor_data)
        .await
}

async fn validate_native_format_with_pot_only(
    probe_client: &Client,
    format: &mut ChosenFormat,
    profile: &NativeClientProfile,
    video_id: &str,
    visitor_data: Option<&str>,
) -> Result<(), String> {
    match validate_chosen_format(probe_client, format).await {
        Ok(()) => Ok(()),
        Err(initial_error) => {
            if !initial_error.may_need_po_token() {
                return Err(initial_error.to_string());
            }
            let Some((token, was_cached)) =
                po_token(profile, PoTokenContext::Gvs, video_id, visitor_data, false).await
            else {
                return Err(initial_error.to_string());
            };
            format.stream_url = if format.is_hls {
                hls_url_with_po_token(&format.stream_url, &token.value)?
            } else {
                url_with_po_token(&format.stream_url, &token.value)?
            };
            format.po_token_expires_at_unix = Some(token.expires_at_unix);
            match validate_chosen_format(probe_client, format).await {
                Ok(()) => Ok(()),
                Err(error) => {
                    invalidate_po_token(profile, PoTokenContext::Gvs, video_id, visitor_data);
                    if !error.may_need_po_token() || !was_cached {
                        return Err(format!("{initial_error}; PO Token retry failed: {error}"));
                    }
                    let Some((fresh_token, _)) =
                        po_token(profile, PoTokenContext::Gvs, video_id, visitor_data, true).await
                    else {
                        return Err(format!("{initial_error}; PO Token retry failed: {error}"));
                    };
                    format.stream_url = if format.is_hls {
                        hls_url_with_po_token(&format.stream_url, &fresh_token.value)?
                    } else {
                        url_with_po_token(&format.stream_url, &fresh_token.value)?
                    };
                    format.po_token_expires_at_unix = Some(fresh_token.expires_at_unix);
                    match validate_chosen_format(probe_client, format).await {
                        Ok(()) => Ok(()),
                        Err(fresh_error) => {
                            invalidate_po_token(
                                profile,
                                PoTokenContext::Gvs,
                                video_id,
                                visitor_data,
                            );
                            Err(format!(
                                "{initial_error}; cached PO Token failed: {error}; fresh PO Token failed: {fresh_error}"
                            ))
                        }
                    }
                }
            }
        }
    }
}

async fn validate_native_request_with_pot(
    probe_client: &Client,
    request: &mut TrackRequest,
    profile: &NativeClientProfile,
    video_id: &str,
    visitor_data: Option<&str>,
    player_url: Option<&str>,
    challenge_detected: Option<&AtomicBool>,
    allow_n_candidate_ack: bool,
) -> Result<(), String> {
    let original_prepared = request.prepared.clone();
    let stream_url = match &request.prepared {
        PreparedSource::Http { stream_url, .. } => stream_url.to_string(),
        PreparedSource::Hls { playlist_url, .. } => playlist_url.to_string(),
    };
    match solve_url_n_challenge(probe_client, &stream_url, player_url, challenge_detected).await {
        Ok(Some(solved)) => {
            match &mut request.prepared {
                PreparedSource::Http { stream_url, .. } => {
                    *stream_url = solved.stream_url.clone().into()
                }
                PreparedSource::Hls { playlist_url, .. } => {
                    *playlist_url = solved.stream_url.clone().into()
                }
            }
            if validate_prepared_source(probe_client, &request.prepared)
                .await
                .is_ok()
            {
                if allow_n_candidate_ack {
                    if let Some(proof) = solved.candidate_proof.as_ref() {
                        acknowledge_validated_js_worker_candidate(proof);
                    } else if let Some(player_url) = player_url {
                        spawn_n_candidate_request_validation(
                            probe_client.clone(),
                            stream_url.clone(),
                            player_url.to_owned(),
                            request.clone(),
                        );
                    }
                }
                tracing::info!(
                    extractor = "native_js",
                    "YouTube selected stream after solving the n challenge"
                );
                return Ok(());
            }
            if allow_n_candidate_ack && let Some(proof) = solved.candidate_proof.as_ref() {
                reject_js_worker_candidate(proof).await;
            }
            if solved.candidate_proof.is_none()
                && let Ok(Some(candidate)) =
                    solve_url_n_challenge_candidate(&stream_url, player_url).await
            {
                match &mut request.prepared {
                    PreparedSource::Http { stream_url, .. } => {
                        *stream_url = candidate.stream_url.clone().into()
                    }
                    PreparedSource::Hls { playlist_url, .. } => {
                        *playlist_url = candidate.stream_url.clone().into()
                    }
                }
                if validate_prepared_source(probe_client, &request.prepared)
                    .await
                    .is_ok()
                {
                    if allow_n_candidate_ack && let Some(proof) = candidate.candidate_proof.as_ref()
                    {
                        acknowledge_validated_js_worker_candidate(proof);
                    }
                    tracing::info!(
                        extractor = "native_js",
                        "YouTube selected stream from the candidate JavaScript worker"
                    );
                    return Ok(());
                }
                if allow_n_candidate_ack && let Some(proof) = candidate.candidate_proof.as_ref() {
                    reject_js_worker_candidate(proof).await;
                }
            }
            request.prepared = original_prepared;
        }
        Ok(None) => {}
        Err(error) => {
            tracing::debug!(
                extractor = "native_js",
                error = %error,
                "YouTube n challenge solve was unavailable"
            );
        }
    }
    validate_native_request_with_pot_only(probe_client, request, profile, video_id, visitor_data)
        .await
}

async fn validate_native_request_with_pot_only(
    probe_client: &Client,
    request: &mut TrackRequest,
    profile: &NativeClientProfile,
    video_id: &str,
    visitor_data: Option<&str>,
) -> Result<(), String> {
    match validate_prepared_source(probe_client, &request.prepared).await {
        Ok(()) => Ok(()),
        Err(initial_error) => {
            if !initial_error.may_need_po_token() {
                return Err(initial_error.to_string());
            }
            let Some((token, was_cached)) =
                po_token(profile, PoTokenContext::Gvs, video_id, visitor_data, false).await
            else {
                return Err(initial_error.to_string());
            };
            add_po_token_to_prepared_source(&mut request.prepared, &token)?;
            match validate_prepared_source(probe_client, &request.prepared).await {
                Ok(()) => Ok(()),
                Err(error) => {
                    invalidate_po_token(profile, PoTokenContext::Gvs, video_id, visitor_data);
                    if !error.may_need_po_token() || !was_cached {
                        return Err(format!("{initial_error}; PO Token retry failed: {error}"));
                    }
                    let Some((fresh_token, _)) =
                        po_token(profile, PoTokenContext::Gvs, video_id, visitor_data, true).await
                    else {
                        return Err(format!("{initial_error}; PO Token retry failed: {error}"));
                    };
                    add_po_token_to_prepared_source(&mut request.prepared, &fresh_token)?;
                    match validate_prepared_source(probe_client, &request.prepared).await {
                        Ok(()) => Ok(()),
                        Err(fresh_error) => {
                            invalidate_po_token(
                                profile,
                                PoTokenContext::Gvs,
                                video_id,
                                visitor_data,
                            );
                            Err(format!(
                                "{initial_error}; cached PO Token failed: {error}; fresh PO Token failed: {fresh_error}"
                            ))
                        }
                    }
                }
            }
        }
    }
}

fn add_po_token_to_prepared_source(
    prepared: &mut PreparedSource,
    token: &PoToken,
) -> Result<(), String> {
    match prepared {
        PreparedSource::Http {
            stream_url,
            expires_at_unix,
            ..
        } => {
            let stream_expires_at_unix = format_url_expiry(stream_url.as_ref());
            *stream_url = url_with_po_token(stream_url.as_ref(), &token.value)?.into();
            *expires_at_unix = earliest_expiry(stream_expires_at_unix, Some(token.expires_at_unix));
        }
        PreparedSource::Hls {
            playlist_url,
            expires_at_unix,
            ..
        } => {
            let stream_expires_at_unix = format_url_expiry(playlist_url.as_ref());
            *playlist_url = hls_url_with_po_token(playlist_url.as_ref(), &token.value)?.into();
            *expires_at_unix = earliest_expiry(stream_expires_at_unix, Some(token.expires_at_unix));
        }
    }
    Ok(())
}

fn url_with_po_token(raw_url: &str, token: &str) -> Result<String, String> {
    url_with_query_value(raw_url, "pot", token)
}

fn url_with_query_value(raw_url: &str, key: &str, value: &str) -> Result<String, String> {
    Url::parse(raw_url).map_err(|error| format!("invalid stream URL: {error}"))?;
    let mut encoded = Url::parse("https://localhost/").expect("static URL should parse");
    encoded.query_pairs_mut().append_pair(key, value);
    let encoded_pair = encoded
        .query()
        .expect("query pair should have been created");

    let (without_fragment, fragment) = raw_url
        .split_once('#')
        .map_or((raw_url, None), |(url, fragment)| (url, Some(fragment)));
    let (base, query) = without_fragment
        .split_once('?')
        .map_or((without_fragment, None), |(base, query)| {
            (base, Some(query))
        });
    let mut pairs = query
        .into_iter()
        .flat_map(|query| query.split('&'))
        .filter(|pair| {
            let existing_key = pair.split_once('=').map_or(*pair, |(key, _)| key);
            !existing_key.eq_ignore_ascii_case(key)
        })
        .filter(|pair| !pair.is_empty())
        .collect::<Vec<_>>();
    pairs.push(encoded_pair);

    let mut updated = format!("{base}?{}", pairs.join("&"));
    if let Some(fragment) = fragment {
        updated.push('#');
        updated.push_str(fragment);
    }
    Ok(updated)
}

fn path_n_challenge(url: &Url) -> Option<String> {
    let segments = url.path().split('/').collect::<Vec<_>>();
    segments.windows(2).find_map(|segments| {
        (segments[0] == "n"
            && !segments[1].is_empty()
            && segments[1].len() <= YOUTUBE_CHALLENGE_VALUE_MAX_BYTES)
            .then(|| segments[1].to_owned())
    })
}

fn url_with_path_n_value(raw_url: &str, value: &str) -> Result<String, String> {
    Url::parse(raw_url).map_err(|error| format!("invalid stream URL: {error}"))?;
    let mut encoded = Url::parse("https://localhost/").expect("static URL should parse");
    {
        let mut path = encoded
            .path_segments_mut()
            .expect("static URL should support path segments");
        path.pop_if_empty();
        path.push(value);
    }
    let encoded_value = encoded.path().trim_start_matches('/');

    let (without_fragment, fragment) = raw_url
        .split_once('#')
        .map_or((raw_url, None), |(url, fragment)| (url, Some(fragment)));
    let (without_query, query) = without_fragment
        .split_once('?')
        .map_or((without_fragment, None), |(url, query)| (url, Some(query)));
    let authority_start = without_query
        .find("://")
        .map(|index| index + 3)
        .ok_or_else(|| "stream URL had no authority".to_owned())?;
    let path_start = without_query[authority_start..]
        .find('/')
        .map(|index| authority_start + index)
        .ok_or_else(|| "stream URL had no path".to_owned())?;
    let (origin, raw_path) = without_query.split_at(path_start);
    let mut segments = raw_path.split('/').collect::<Vec<_>>();
    let Some(index) = segments
        .windows(2)
        .position(|segments| segments[0] == "n" && !segments[1].is_empty())
    else {
        return Err("stream URL had no path n challenge".to_owned());
    };
    segments[index + 1] = encoded_value;
    let mut updated = format!("{origin}{}", segments.join("/"));
    if let Some(query) = query {
        updated.push('?');
        updated.push_str(query);
    }
    if let Some(fragment) = fragment {
        updated.push('#');
        updated.push_str(fragment);
    }
    Ok(updated)
}

fn hls_url_with_po_token(raw_url: &str, token: &str) -> Result<String, String> {
    let mut url = Url::parse(raw_url).map_err(|error| format!("invalid stream URL: {error}"))?;
    let replaces_existing = url
        .path_segments()
        .map(|segments| segments.collect::<Vec<_>>())
        .is_some_and(|segments| {
            segments.len() >= 2 && segments[segments.len() - 2].eq_ignore_ascii_case("pot")
        });
    let mut segments = url
        .path_segments_mut()
        .map_err(|_| "HLS stream URL cannot contain path segments".to_owned())?;
    segments.pop_if_empty();
    if replaces_existing {
        segments.pop();
        segments.pop();
    }
    segments.push("pot");
    segments.push(token);
    drop(segments);
    Ok(url.to_string())
}

async fn validate_prepared_source(
    probe_client: &Client,
    prepared: &PreparedSource,
) -> Result<(), StreamValidationError> {
    let (raw_url, prepared_headers, range_mode, is_hls) = match prepared {
        PreparedSource::Http {
            stream_url,
            headers,
            range_mode,
            ..
        } => (
            stream_url.as_ref(),
            headers.as_ref(),
            Some(*range_mode),
            false,
        ),
        PreparedSource::Hls {
            playlist_url,
            headers,
            ..
        } => (playlist_url.as_ref(), headers.as_ref(), None, true),
    };
    let headers =
        prepared_headers_to_map(prepared_headers).map_err(StreamValidationError::InvalidRequest)?;
    let mut url = Url::parse(raw_url).map_err(|error| {
        StreamValidationError::InvalidRequest(format!("invalid stream URL: {error}"))
    })?;
    let mut request = probe_client
        .get(url.clone())
        .headers(headers)
        .timeout(YOUTUBE_STREAM_VALIDATION_TIMEOUT);
    if !is_hls {
        if range_mode == Some(PreparedRangeMode::QueryParam) {
            let range = YOUTUBE_STREAM_VALIDATION_RANGE
                .strip_prefix("bytes=")
                .unwrap_or(YOUTUBE_STREAM_VALIDATION_RANGE);
            url.query_pairs_mut().append_pair("range", range);
            request = probe_client
                .get(url)
                .headers(
                    prepared_headers_to_map(prepared_headers)
                        .map_err(StreamValidationError::InvalidRequest)?,
                )
                .timeout(YOUTUBE_STREAM_VALIDATION_TIMEOUT);
        } else {
            request = request.header(RANGE, YOUTUBE_STREAM_VALIDATION_RANGE);
        }
    }

    let mut response = request
        .send()
        .await
        .map_err(StreamValidationError::Request)?;
    let status = response.status();
    if !status.is_success() {
        return Err(StreamValidationError::HttpStatus(status));
    }

    if is_hls {
        let mut prefix = Vec::with_capacity(256);
        while prefix.len() < 256 {
            let Some(chunk) = response
                .chunk()
                .await
                .map_err(StreamValidationError::Request)?
            else {
                break;
            };
            prefix.extend_from_slice(&chunk[..chunk.len().min(256 - prefix.len())]);
            if prefix.windows(7).any(|window| window == b"#EXTM3U") {
                return Ok(());
            }
        }
        Err(StreamValidationError::InvalidHlsBody)
    } else {
        let mut received = 0;
        while received < 1_024 {
            let Some(chunk) = response
                .chunk()
                .await
                .map_err(StreamValidationError::Request)?
            else {
                break;
            };
            received += chunk.len();
        }
        if received >= 1_024 {
            Ok(())
        } else {
            Err(StreamValidationError::InsufficientBody(received))
        }
    }
}

fn prepared_headers_to_map(headers: &[PreparedHeader]) -> Result<HeaderMap, String> {
    let mut prepared = HeaderMap::new();
    for header in headers {
        let name = HeaderName::from_bytes(header.name.as_bytes())
            .map_err(|_| format!("invalid header name: {}", header.name))?;
        let value = HeaderValue::from_str(header.value.as_ref())
            .map_err(|_| format!("invalid header value for {}", header.name))?;
        prepared.insert(name, value);
    }
    Ok(prepared)
}

fn choose_playable_format(
    formats: &[VideoFormat],
    hls_manifest_url: &Option<String>,
    options: &VideoOptions,
) -> Result<ChosenFormat, String> {
    let playable_formats: Vec<VideoFormat> = formats
        .iter()
        .filter(|format| !format.url.is_empty())
        .cloned()
        .collect();

    if let Ok(format) = choose_format(&playable_formats, options) {
        let content_length = format.content_length.clone();
        return Ok(ChosenFormat {
            stream_url: format.url,
            content_length: content_length.clone(),
            is_hls: format.is_hls,
            headers: web_stream_headers(),
            range_chunk_size: (!format.is_hls && content_length.is_some())
                .then_some(YOUTUBE_RANGE_CHUNK_SIZE),
            po_token_expires_at_unix: None,
        });
    }

    if let Some(hls_manifest_url) = hls_manifest_url.as_ref().filter(|url| !url.is_empty()) {
        return Ok(ChosenFormat {
            stream_url: hls_manifest_url.clone(),
            content_length: None,
            is_hls: true,
            headers: web_stream_headers(),
            range_chunk_size: None,
            po_token_expires_at_unix: None,
        });
    }

    let playable_muxed_formats: Vec<VideoFormat> = playable_formats
        .iter()
        .filter(|format| format.has_audio)
        .cloned()
        .collect();
    if let Some(format) = playable_muxed_formats.first() {
        let content_length = format.content_length.clone();
        return Ok(ChosenFormat {
            stream_url: format.url.clone(),
            content_length: content_length.clone(),
            is_hls: format.is_hls,
            headers: web_stream_headers(),
            range_chunk_size: (!format.is_hls && content_length.is_some())
                .then_some(YOUTUBE_RANGE_CHUNK_SIZE),
            po_token_expires_at_unix: None,
        });
    }

    Err("YouTube did not expose a playable stream URL".to_owned())
}

fn youtube_options(client: Client) -> VideoOptions {
    VideoOptions {
        quality: VideoQuality::HighestAudio,
        filter: VideoSearchOptions::Audio,
        request_options: rusty_ytdl::RequestOptions {
            client: Some(client),
            ..Default::default()
        },
        ..Default::default()
    }
}

fn web_stream_headers() -> Vec<PreparedHeader> {
    vec![
        PreparedHeader::new(
            "User-Agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/135.0.0.0 Safari/537.36",
        ),
        PreparedHeader::new("Referer", "https://www.youtube.com/"),
        PreparedHeader::new("Origin", "https://www.youtube.com"),
    ]
}

fn native_stream_headers(profile: &NativeClientProfile) -> Vec<PreparedHeader> {
    vec![PreparedHeader::new(
        "User-Agent",
        profile.user_agent.clone(),
    )]
}

async fn fetch_android_vr_audio_format(
    probe_client: &Client,
    canonical_url: &str,
    video_id: &str,
) -> Result<Option<ChosenFormat>, ResolveError> {
    let visitor_profile = visitor_native_client_profile();
    let vr_response =
        fetch_android_vr_player_response(probe_client, canonical_url, video_id, None).await?;
    if let Some((response, session)) = vr_response.as_ref()
        && let Some(mut format) = response.streaming_data.as_ref().and_then(|streaming_data| {
            chosen_format_from_android_streaming_data(
                streaming_data,
                native_stream_headers(&visitor_profile),
            )
        })
    {
        match validate_native_format_with_pot(
            probe_client,
            &mut format,
            &visitor_profile,
            video_id,
            Some(session.visitor_data.as_str()),
            session.player_url.as_deref(),
            None,
            !response.has_cipher_stream(),
        )
        .await
        {
            Ok(()) => {
                if response.has_cipher_stream()
                    && let Some(player_url) = session.player_url.as_deref()
                {
                    spawn_cipher_candidate_format_validation(
                        probe_client.clone(),
                        response.clone(),
                        visitor_profile.clone(),
                        video_id.to_owned(),
                        Some(session.visitor_data.clone()),
                        player_url.to_owned(),
                    );
                }
                tracing::info!(
                    extractor = "native",
                    strategy = "android_vr_visitor",
                    "YouTube selected verified visitor-bound stream"
                );
                return Ok(Some(format));
            }
            Err(error) => {
                if let Ok(Some(cipher_format)) = validated_cipher_format(
                    probe_client,
                    response,
                    &visitor_profile,
                    video_id,
                    Some(session.visitor_data.as_str()),
                    session.player_url.as_deref(),
                    None,
                )
                .await
                {
                    tracing::info!(
                        extractor = "native_js",
                        strategy = "android_vr_visitor_cipher",
                        "YouTube selected verified cipher fallback stream"
                    );
                    return Ok(Some(cipher_format));
                }
                tracing::warn!(
                    extractor = "native",
                    strategy = "android_vr_visitor",
                    error = %error,
                    "YouTube rejected visitor-bound stream candidate"
                );
            }
        }
    }

    let profiles = native_client_profiles();
    for profile in profiles.iter() {
        let response = match fetch_direct_native_player_response(
            probe_client,
            video_id,
            profile,
            vr_response
                .as_ref()
                .and_then(|(_, session)| session.player_url.as_deref()),
            None,
        )
        .await
        {
            Ok(response) => response,
            Err(error) => {
                tracing::debug!(
                    extractor = "native",
                    strategy = profile.id.as_str(),
                    error = %error,
                    "YouTube native player request failed"
                );
                continue;
            }
        };
        let Some(mut format) = response
            .as_ref()
            .and_then(|response| response.streaming_data.as_ref())
            .and_then(|streaming_data| {
                chosen_format_from_android_streaming_data(
                    streaming_data,
                    native_stream_headers(profile),
                )
            })
        else {
            continue;
        };
        match validate_native_format_with_pot(
            probe_client,
            &mut format,
            profile,
            video_id,
            None,
            vr_response
                .as_ref()
                .and_then(|(_, session)| session.player_url.as_deref()),
            None,
            response
                .as_ref()
                .is_none_or(|response| !response.has_cipher_stream()),
        )
        .await
        {
            Ok(()) => {
                if let Some(response) = response
                    .as_ref()
                    .filter(|response| response.has_cipher_stream())
                    && let Some(player_url) = vr_response
                        .as_ref()
                        .and_then(|(_, session)| session.player_url.as_deref())
                {
                    spawn_cipher_candidate_format_validation(
                        probe_client.clone(),
                        response.clone(),
                        profile.clone(),
                        video_id.to_owned(),
                        None,
                        player_url.to_owned(),
                    );
                }
                tracing::info!(
                    extractor = "native",
                    strategy = profile.id.as_str(),
                    "YouTube selected verified native stream"
                );
                return Ok(Some(format));
            }
            Err(error) => {
                if let Some(response) = response.as_ref()
                    && let Ok(Some(cipher_format)) = validated_cipher_format(
                        probe_client,
                        response,
                        profile,
                        video_id,
                        None,
                        vr_response
                            .as_ref()
                            .and_then(|(_, session)| session.player_url.as_deref()),
                        None,
                    )
                    .await
                {
                    tracing::info!(
                        extractor = "native_js",
                        strategy = profile.id.as_str(),
                        "YouTube selected verified cipher fallback stream"
                    );
                    return Ok(Some(cipher_format));
                }
                tracing::warn!(
                    extractor = "native",
                    strategy = profile.id.as_str(),
                    error = %error,
                    "YouTube rejected native stream candidate"
                );
            }
        }
    }

    Ok(None)
}

async fn fetch_android_vr_track_request_fast(
    raw_url: &str,
    probe_client: &Client,
) -> Result<Option<TrackRequest>, ResolveError> {
    let Some(video_id) = youtube_video_id_from_url(raw_url) else {
        return Ok(None);
    };

    let challenge_detected = AtomicBool::new(false);
    let future =
        fetch_android_vr_track_request(probe_client, raw_url, &video_id, Some(&challenge_detected));
    tokio::pin!(future);
    if youtube_pot::is_configured() {
        return match tokio::time::timeout(YOUTUBE_PO_TOKEN_FAST_PATH_TIMEOUT, future).await {
            Ok(result) => result,
            Err(_) => Ok(None),
        };
    }
    match tokio::time::timeout(YOUTUBE_NATIVE_FAST_PATH_TIMEOUT, future.as_mut()).await {
        Ok(result) => result,
        Err(_) if challenge_detected.load(Ordering::Relaxed) => {
            match tokio::time::timeout(
                YOUTUBE_JS_SLOW_PATH_TIMEOUT - YOUTUBE_NATIVE_FAST_PATH_TIMEOUT,
                future,
            )
            .await
            {
                Ok(result) => result,
                Err(_) => Ok(None),
            }
        }
        Err(_) => Ok(None),
    }
}

async fn fetch_android_vr_track_request(
    probe_client: &Client,
    raw_url: &str,
    video_id: &str,
    challenge_detected: Option<&AtomicBool>,
) -> Result<Option<TrackRequest>, ResolveError> {
    let visitor_profile = visitor_native_client_profile();
    let vr_response = fetch_android_vr_player_response(
        probe_client,
        &format!("https://www.youtube.com/watch?v={video_id}"),
        video_id,
        challenge_detected,
    )
    .await?;
    if let Some((response, session)) = vr_response.as_ref()
        && let Some(mut request) = native_track_request_from_response(
            Some(response.clone()),
            raw_url,
            video_id,
            native_stream_headers(&visitor_profile),
        )
    {
        match validate_native_request_with_pot(
            probe_client,
            &mut request,
            &visitor_profile,
            video_id,
            Some(session.visitor_data.as_str()),
            session.player_url.as_deref(),
            challenge_detected,
            !response.has_cipher_stream(),
        )
        .await
        {
            Ok(()) => {
                if response.has_cipher_stream()
                    && let Some(player_url) = session.player_url.as_deref()
                {
                    spawn_cipher_candidate_request_validation(
                        probe_client.clone(),
                        response.clone(),
                        raw_url.to_owned(),
                        visitor_profile.clone(),
                        video_id.to_owned(),
                        Some(session.visitor_data.clone()),
                        player_url.to_owned(),
                    );
                }
                tracing::info!(
                    extractor = "native",
                    strategy = "android_vr_visitor",
                    video_key = %request.canonical_key,
                    "YouTube selected verified visitor-bound native path"
                );
                return Ok(Some(request));
            }
            Err(error) => {
                if let Ok(Some(cipher_request)) = validated_cipher_request(
                    probe_client,
                    response,
                    raw_url,
                    &visitor_profile,
                    video_id,
                    Some(session.visitor_data.as_str()),
                    session.player_url.as_deref(),
                    challenge_detected,
                )
                .await
                {
                    tracing::info!(
                        extractor = "native_js",
                        strategy = "android_vr_visitor_cipher",
                        video_key = %cipher_request.canonical_key,
                        "YouTube selected verified cipher fallback path"
                    );
                    return Ok(Some(cipher_request));
                }
                tracing::warn!(
                    extractor = "native",
                    strategy = "android_vr_visitor",
                    error = %error,
                    "YouTube visitor-bound native path returned an unplayable stream"
                );
            }
        }
    }

    let profiles = native_client_profiles();
    for profile in profiles.iter() {
        let response = match fetch_direct_native_player_response(
            probe_client,
            video_id,
            profile,
            vr_response
                .as_ref()
                .and_then(|(_, session)| session.player_url.as_deref()),
            challenge_detected,
        )
        .await
        {
            Ok(response) => response,
            Err(error) => {
                tracing::debug!(
                    extractor = "native",
                    strategy = profile.id.as_str(),
                    error = %error,
                    "YouTube native metadata request failed"
                );
                continue;
            }
        };
        let Some(mut request) = native_track_request_from_response(
            response.clone(),
            raw_url,
            video_id,
            native_stream_headers(profile),
        ) else {
            continue;
        };
        match validate_native_request_with_pot(
            probe_client,
            &mut request,
            profile,
            video_id,
            None,
            vr_response
                .as_ref()
                .and_then(|(_, session)| session.player_url.as_deref()),
            challenge_detected,
            response
                .as_ref()
                .is_none_or(|response| !response.has_cipher_stream()),
        )
        .await
        {
            Ok(()) => {
                if let Some(response) = response
                    .as_ref()
                    .filter(|response| response.has_cipher_stream())
                    && let Some(player_url) = vr_response
                        .as_ref()
                        .and_then(|(_, session)| session.player_url.as_deref())
                {
                    spawn_cipher_candidate_request_validation(
                        probe_client.clone(),
                        response.clone(),
                        raw_url.to_owned(),
                        profile.clone(),
                        video_id.to_owned(),
                        None,
                        player_url.to_owned(),
                    );
                }
                tracing::info!(
                    extractor = "native",
                    strategy = profile.id.as_str(),
                    video_key = %request.canonical_key,
                    "YouTube selected verified native fast path"
                );
                return Ok(Some(request));
            }
            Err(error) => {
                if let Some(response) = response.as_ref()
                    && let Ok(Some(cipher_request)) = validated_cipher_request(
                        probe_client,
                        response,
                        raw_url,
                        profile,
                        video_id,
                        None,
                        vr_response
                            .as_ref()
                            .and_then(|(_, session)| session.player_url.as_deref()),
                        challenge_detected,
                    )
                    .await
                {
                    tracing::info!(
                        extractor = "native_js",
                        strategy = profile.id.as_str(),
                        video_key = %cipher_request.canonical_key,
                        "YouTube selected verified cipher fallback path"
                    );
                    return Ok(Some(cipher_request));
                }
                tracing::warn!(
                    extractor = "native",
                    strategy = profile.id.as_str(),
                    error = %error,
                    "YouTube native fast path returned an unplayable stream"
                );
            }
        }
    }

    Ok(None)
}

fn native_track_request_from_response(
    response: Option<AndroidPlayerResponse>,
    raw_url: &str,
    video_id: &str,
    stream_headers: Vec<PreparedHeader>,
) -> Option<TrackRequest> {
    let response = response?;
    let details = response.video_details?;
    let streaming_data = response.streaming_data?;
    let format = chosen_format_from_android_streaming_data(&streaming_data, stream_headers)?;

    let expires_at_unix = format_url_expiry(format.stream_url.as_ref());
    let content_length = format.content_length.as_deref().and_then(|value| {
        value
            .parse::<u64>()
            .ok()
            .or_else(|| parse_content_length_from_url(format.stream_url.as_ref()))
    });
    let is_live_content = details.is_live_content;
    let prepared =
        prepared_source_from_format(format, is_live_content, content_length, expires_at_unix);
    let canonical_video_id = if details.video_id.is_empty() {
        video_id
    } else {
        details.video_id.as_str()
    };
    let canonical_url = format!("https://www.youtube.com/watch?v={canonical_video_id}");
    let title = if details.title.is_empty() {
        canonical_url.clone()
    } else {
        details.title
    };
    let author = if details.author.is_empty() {
        "YouTube".to_owned()
    } else {
        details.author
    };
    let thumbnail_url = details
        .thumbnail
        .as_ref()
        .and_then(|thumbnail| pick_android_thumbnail(&thumbnail.thumbnails));
    let duration = details
        .length_seconds
        .as_deref()
        .and_then(|length| parse_duration(length, is_live_content));

    Some(TrackRequest::new(
        "youtube",
        format!("youtube:video:{canonical_video_id}"),
        raw_url.to_owned(),
        canonical_url.clone(),
        canonical_url.clone(),
        prepared,
        TrackMetadata::new(title, author, canonical_url, thumbnail_url, duration),
    ))
}

async fn fetch_android_vr_player_response(
    probe_client: &Client,
    canonical_url: &str,
    video_id: &str,
    challenge_detected: Option<&AtomicBool>,
) -> Result<Option<(AndroidPlayerResponse, CachedVisitorSession)>, ResolveError> {
    let (session, was_cached) = visitor_session(probe_client, canonical_url, false).await?;
    let response =
        fetch_visitor_bound_player_response(probe_client, video_id, &session, challenge_detected)
            .await?;
    if !was_cached || !response.requires_fresh_visitor_session() {
        return Ok(Some((response, session)));
    }

    let (session, _) = visitor_session(probe_client, canonical_url, true).await?;
    let response =
        fetch_visitor_bound_player_response(probe_client, video_id, &session, challenge_detected)
            .await?;
    Ok(Some((response, session)))
}

async fn fetch_visitor_bound_player_response(
    probe_client: &Client,
    video_id: &str,
    session: &CachedVisitorSession,
    challenge_detected: Option<&AtomicBool>,
) -> Result<AndroidPlayerResponse, ResolveError> {
    let visitor_profile = visitor_native_client_profile();
    let response = send_native_player_request(
        probe_client,
        video_id,
        &visitor_profile,
        Some(session.signature_timestamp),
        Some(session.visitor_data.as_str()),
        None,
    )
    .await?;
    let response = solve_native_player_challenges(
        probe_client,
        response,
        session.player_url.as_deref(),
        challenge_detected,
    )
    .await;
    if response.has_playable_stream() {
        return Ok(response);
    }
    let Some((player_token, was_cached)) = po_token(
        &visitor_profile,
        PoTokenContext::Player,
        video_id,
        Some(session.visitor_data.as_str()),
        false,
    )
    .await
    else {
        return Ok(response);
    };
    let response = send_native_player_request(
        probe_client,
        video_id,
        &visitor_profile,
        Some(session.signature_timestamp),
        Some(session.visitor_data.as_str()),
        Some(player_token.value.as_str()),
    )
    .await?;
    let response = solve_native_player_challenges(
        probe_client,
        response,
        session.player_url.as_deref(),
        challenge_detected,
    )
    .await;
    if response.has_playable_stream() {
        return Ok(response);
    }
    invalidate_po_token(
        &visitor_profile,
        PoTokenContext::Player,
        video_id,
        Some(session.visitor_data.as_str()),
    );
    if !was_cached {
        return Ok(response);
    }
    let Some((fresh_token, _)) = po_token(
        &visitor_profile,
        PoTokenContext::Player,
        video_id,
        Some(session.visitor_data.as_str()),
        true,
    )
    .await
    else {
        return Ok(response);
    };
    let fresh_response = send_native_player_request(
        probe_client,
        video_id,
        &visitor_profile,
        Some(session.signature_timestamp),
        Some(session.visitor_data.as_str()),
        Some(fresh_token.value.as_str()),
    )
    .await?;
    let fresh_response = solve_native_player_challenges(
        probe_client,
        fresh_response,
        session.player_url.as_deref(),
        challenge_detected,
    )
    .await;
    if !fresh_response.has_playable_stream() {
        invalidate_po_token(
            &visitor_profile,
            PoTokenContext::Player,
            video_id,
            Some(session.visitor_data.as_str()),
        );
    }
    Ok(fresh_response)
}

async fn send_native_player_request(
    probe_client: &Client,
    video_id: &str,
    profile: &NativeClientProfile,
    signature_timestamp: Option<u64>,
    visitor_data: Option<&str>,
    player_token: Option<&str>,
) -> Result<AndroidPlayerResponse, ResolveError> {
    probe_client
        .post("https://youtubei.googleapis.com/youtubei/v1/player?prettyPrint=false")
        .headers(native_api_headers(profile, visitor_data)?)
        .json(&native_player_request(
            video_id,
            profile,
            signature_timestamp,
            visitor_data,
            player_token,
        ))
        .send()
        .await
        .map_err(ResolveError::Request)?
        .error_for_status()
        .map_err(ResolveError::Request)?
        .json::<AndroidPlayerResponse>()
        .await
        .map_err(ResolveError::Request)
}

async fn visitor_session(
    probe_client: &Client,
    canonical_url: &str,
    force_refresh: bool,
) -> Result<(CachedVisitorSession, bool), ResolveError> {
    let cache = YOUTUBE_VISITOR_SESSION.get_or_init(|| AsyncRwLock::new(None));
    if !force_refresh {
        let cached = cache.read().await;
        if let Some(session) = cached.as_ref()
            && session.cached_at.elapsed() <= YOUTUBE_VISITOR_SESSION_TTL
        {
            return Ok((session.clone(), true));
        }
    }

    let watch_html = probe_client
        .get(canonical_url)
        .query(&[("hl", "en")])
        .send()
        .await
        .map_err(ResolveError::Request)?
        .error_for_status()
        .map_err(ResolveError::Request)?
        .text()
        .await
        .map_err(ResolveError::Request)?;
    let Some(signature_timestamp) = extract_signature_timestamp(&watch_html) else {
        return Err(ResolveError::Parse(
            "YouTube visitor session was missing signature timestamp".to_owned(),
        ));
    };
    let Some(visitor_data) = extract_visitor_data(&watch_html) else {
        return Err(ResolveError::Parse(
            "YouTube visitor session was missing visitor data".to_owned(),
        ));
    };
    let session = CachedVisitorSession {
        visitor_data,
        signature_timestamp,
        player_url: extract_player_url(&watch_html),
        cached_at: Instant::now(),
    };
    let mut cached = cache.write().await;
    if !force_refresh
        && let Some(existing) = cached.as_ref()
        && existing.cached_at.elapsed() <= YOUTUBE_VISITOR_SESSION_TTL
    {
        return Ok((existing.clone(), true));
    }
    *cached = Some(session.clone());
    Ok((session, false))
}

async fn solve_native_player_challenges(
    probe_client: &Client,
    mut response: AndroidPlayerResponse,
    player_url: Option<&str>,
    challenge_detected: Option<&AtomicBool>,
) -> AndroidPlayerResponse {
    if response.has_playable_stream() || !response.has_cipher_stream() {
        return response;
    }
    if let Some(challenge_detected) = challenge_detected {
        challenge_detected.store(true, Ordering::Relaxed);
    }
    let Some(player_url) = player_url else {
        tracing::debug!(
            extractor = "native_js",
            "YouTube returned ciphered streams without a Player JavaScript URL"
        );
        return response;
    };
    match solve_android_cipher_formats(probe_client, &mut response, player_url).await {
        Ok(solved) => {
            tracing::info!(
                extractor = "native_js",
                solved,
                "YouTube solved Player JavaScript stream challenges"
            );
        }
        Err(error) => {
            tracing::warn!(
                extractor = "native_js",
                error = %error,
                "YouTube Player JavaScript challenge solver failed"
            );
        }
    }
    response
}

async fn solved_cipher_player_response(
    probe_client: &Client,
    response: &AndroidPlayerResponse,
    player_url: Option<&str>,
    challenge_detected: Option<&AtomicBool>,
) -> Result<Option<SolvedCipherPlayerResponse>, String> {
    let Some(player_url) = player_url else {
        return Ok(None);
    };
    let Some(mut cipher_response) = cipher_only_player_response(response) else {
        return Ok(None);
    };
    if let Some(challenge_detected) = challenge_detected {
        challenge_detected.store(true, Ordering::Relaxed);
    }
    match solve_android_cipher_formats_with_worker(
        probe_client,
        &mut cipher_response,
        player_url,
        CipherWorkerChoice::Current,
    )
    .await
    {
        Ok(solved) if solved.solved > 0 && cipher_response.has_playable_stream() => {
            Ok(Some(SolvedCipherPlayerResponse {
                response: cipher_response,
                candidate_proof: None,
            }))
        }
        Ok(_) | Err(_) => {
            solved_cipher_player_response_candidate(probe_client, response, player_url).await
        }
    }
}

fn cipher_only_player_response(response: &AndroidPlayerResponse) -> Option<AndroidPlayerResponse> {
    let mut cipher_response = response.clone();
    let streaming_data = cipher_response.streaming_data.as_mut()?;
    streaming_data.adaptive_formats.retain(|format| {
        format.mime_type.starts_with("audio/")
            && format
                .signature_cipher
                .as_ref()
                .is_some_and(|cipher| !cipher.is_empty())
    });
    for format in &mut streaming_data.adaptive_formats {
        format.url = None;
    }
    streaming_data.hls_manifest_url = None;
    (!streaming_data.adaptive_formats.is_empty()).then_some(cipher_response)
}

async fn solved_cipher_player_response_candidate(
    probe_client: &Client,
    response: &AndroidPlayerResponse,
    player_url: &str,
) -> Result<Option<SolvedCipherPlayerResponse>, String> {
    let Some(mut cipher_response) = cipher_only_player_response(response) else {
        return Ok(None);
    };
    let solved = solve_android_cipher_formats_with_worker(
        probe_client,
        &mut cipher_response,
        player_url,
        CipherWorkerChoice::Candidate,
    )
    .await?;
    Ok(
        (solved.solved > 0 && cipher_response.has_playable_stream()).then_some(
            SolvedCipherPlayerResponse {
                response: cipher_response,
                candidate_proof: solved.candidate_proof,
            },
        ),
    )
}

fn spawn_cipher_candidate_format_validation(
    probe_client: Client,
    response: AndroidPlayerResponse,
    profile: NativeClientProfile,
    video_id: String,
    visitor_data: Option<String>,
    player_url: String,
) {
    tokio::spawn(async move {
        let Ok(Some(candidate)) =
            solved_cipher_player_response_candidate(&probe_client, &response, &player_url).await
        else {
            return;
        };
        let Some(mut format) =
            candidate
                .response
                .streaming_data
                .as_ref()
                .and_then(|streaming_data| {
                    chosen_format_from_android_streaming_data(
                        streaming_data,
                        native_stream_headers(&profile),
                    )
                })
        else {
            return;
        };
        let validation = validate_native_format_with_pot_only(
            &probe_client,
            &mut format,
            &profile,
            &video_id,
            visitor_data.as_deref(),
        )
        .await;
        if let Some(proof) = candidate.candidate_proof.as_ref() {
            if validation.is_ok() {
                acknowledge_validated_js_worker_candidate(proof);
            } else {
                reject_js_worker_candidate(proof).await;
            }
        }
    });
}

fn spawn_cipher_candidate_request_validation(
    probe_client: Client,
    response: AndroidPlayerResponse,
    raw_url: String,
    profile: NativeClientProfile,
    video_id: String,
    visitor_data: Option<String>,
    player_url: String,
) {
    tokio::spawn(async move {
        let Ok(Some(candidate)) =
            solved_cipher_player_response_candidate(&probe_client, &response, &player_url).await
        else {
            return;
        };
        let Some(mut request) = native_track_request_from_response(
            Some(candidate.response),
            &raw_url,
            &video_id,
            native_stream_headers(&profile),
        ) else {
            return;
        };
        let validation = validate_native_request_with_pot_only(
            &probe_client,
            &mut request,
            &profile,
            &video_id,
            visitor_data.as_deref(),
        )
        .await;
        if let Some(proof) = candidate.candidate_proof.as_ref() {
            if validation.is_ok() {
                acknowledge_validated_js_worker_candidate(proof);
            } else {
                reject_js_worker_candidate(proof).await;
            }
        }
    });
}

async fn validated_cipher_format(
    probe_client: &Client,
    response: &AndroidPlayerResponse,
    profile: &NativeClientProfile,
    video_id: &str,
    visitor_data: Option<&str>,
    player_url: Option<&str>,
    challenge_detected: Option<&AtomicBool>,
) -> Result<Option<ChosenFormat>, String> {
    let Some(solved) =
        solved_cipher_player_response(probe_client, response, player_url, challenge_detected)
            .await?
    else {
        return Ok(None);
    };
    let Some(mut format) = solved
        .response
        .streaming_data
        .as_ref()
        .and_then(|streaming_data| {
            chosen_format_from_android_streaming_data(
                streaming_data,
                native_stream_headers(profile),
            )
        })
    else {
        return Ok(None);
    };
    let validation = validate_native_format_with_pot_only(
        probe_client,
        &mut format,
        profile,
        video_id,
        visitor_data,
    )
    .await;
    match validation {
        Ok(()) => {
            if let Some(proof) = solved.candidate_proof.as_ref() {
                acknowledge_validated_js_worker_candidate(proof);
            } else if let Some(player_url) = player_url {
                spawn_cipher_candidate_format_validation(
                    probe_client.clone(),
                    response.clone(),
                    profile.clone(),
                    video_id.to_owned(),
                    visitor_data.map(str::to_owned),
                    player_url.to_owned(),
                );
            }
            Ok(Some(format))
        }
        Err(current_error) if solved.candidate_proof.is_none() => {
            let Some(player_url) = player_url else {
                return Err(current_error);
            };
            let Some(candidate) =
                solved_cipher_player_response_candidate(probe_client, response, player_url).await?
            else {
                return Err(current_error);
            };
            let Some(mut candidate_format) =
                candidate
                    .response
                    .streaming_data
                    .as_ref()
                    .and_then(|streaming_data| {
                        chosen_format_from_android_streaming_data(
                            streaming_data,
                            native_stream_headers(profile),
                        )
                    })
            else {
                return Err(current_error);
            };
            let candidate_validation = validate_native_format_with_pot_only(
                probe_client,
                &mut candidate_format,
                profile,
                video_id,
                visitor_data,
            )
            .await;
            if let Err(error) = candidate_validation {
                if let Some(proof) = candidate.candidate_proof.as_ref() {
                    reject_js_worker_candidate(proof).await;
                }
                return Err(error);
            }
            if let Some(proof) = candidate.candidate_proof.as_ref() {
                acknowledge_validated_js_worker_candidate(proof);
            }
            Ok(Some(candidate_format))
        }
        Err(error) => {
            if let Some(proof) = solved.candidate_proof.as_ref() {
                reject_js_worker_candidate(proof).await;
            }
            Err(error)
        }
    }
}

async fn validated_cipher_request(
    probe_client: &Client,
    response: &AndroidPlayerResponse,
    raw_url: &str,
    profile: &NativeClientProfile,
    video_id: &str,
    visitor_data: Option<&str>,
    player_url: Option<&str>,
    challenge_detected: Option<&AtomicBool>,
) -> Result<Option<TrackRequest>, String> {
    let Some(solved) =
        solved_cipher_player_response(probe_client, response, player_url, challenge_detected)
            .await?
    else {
        return Ok(None);
    };
    let Some(mut request) = native_track_request_from_response(
        Some(solved.response),
        raw_url,
        video_id,
        native_stream_headers(profile),
    ) else {
        return Ok(None);
    };
    let validation = validate_native_request_with_pot_only(
        probe_client,
        &mut request,
        profile,
        video_id,
        visitor_data,
    )
    .await;
    match validation {
        Ok(()) => {
            if let Some(proof) = solved.candidate_proof.as_ref() {
                acknowledge_validated_js_worker_candidate(proof);
            } else if let Some(player_url) = player_url {
                spawn_cipher_candidate_request_validation(
                    probe_client.clone(),
                    response.clone(),
                    raw_url.to_owned(),
                    profile.clone(),
                    video_id.to_owned(),
                    visitor_data.map(str::to_owned),
                    player_url.to_owned(),
                );
            }
            Ok(Some(request))
        }
        Err(current_error) if solved.candidate_proof.is_none() => {
            let Some(player_url) = player_url else {
                return Err(current_error);
            };
            let Some(candidate) =
                solved_cipher_player_response_candidate(probe_client, response, player_url).await?
            else {
                return Err(current_error);
            };
            let Some(mut candidate_request) = native_track_request_from_response(
                Some(candidate.response),
                raw_url,
                video_id,
                native_stream_headers(profile),
            ) else {
                return Err(current_error);
            };
            let candidate_validation = validate_native_request_with_pot_only(
                probe_client,
                &mut candidate_request,
                profile,
                video_id,
                visitor_data,
            )
            .await;
            if let Err(error) = candidate_validation {
                if let Some(proof) = candidate.candidate_proof.as_ref() {
                    reject_js_worker_candidate(proof).await;
                }
                return Err(error);
            }
            if let Some(proof) = candidate.candidate_proof.as_ref() {
                acknowledge_validated_js_worker_candidate(proof);
            }
            Ok(Some(candidate_request))
        }
        Err(error) => {
            if let Some(proof) = solved.candidate_proof.as_ref() {
                reject_js_worker_candidate(proof).await;
            }
            Err(error)
        }
    }
}

async fn run_youtube_js_solver(
    player_url: &str,
    player_source: Arc<str>,
    inputs: Vec<ChallengeInput>,
) -> Result<Vec<ChallengeOutput>, String> {
    run_youtube_js_solver_isolated(player_url, player_source, inputs)
        .await?
        .into_iter()
        .collect()
}

async fn run_youtube_js_solver_isolated(
    player_url: &str,
    player_source: Arc<str>,
    inputs: Vec<ChallengeInput>,
) -> Result<Vec<Result<ChallengeOutput, String>>, String> {
    let player_key = youtube_player_key(player_url, player_source.as_ref());
    let worker = YOUTUBE_JS_WORKER.get_or_init(|| AsyncMutex::new(JsWorkerSupervisor::default()));
    let routes = {
        let mut supervisor = tokio::time::timeout(YOUTUBE_JS_WORKER_QUEUE_TIMEOUT, worker.lock())
            .await
            .map_err(|_| "Player JavaScript solver queue timed out".to_owned())?;
        supervisor.routes(&player_key, true)?
    };
    let current_key = WorkerPlayerKey {
        executable_identity: routes.current.executable.identity.clone(),
        player_key: player_key.clone(),
    };
    let current = solve_with_js_worker_lane(
        routes.current,
        &player_key,
        player_source.as_ref(),
        &inputs,
        YOUTUBE_JS_WORKER_EXECUTION_TIMEOUT,
    )
    .await;
    {
        let mut supervisor = worker.lock().await;
        match &current {
            Ok(_) => {
                supervisor.failure_until.remove(&current_key);
            }
            Err(_) => supervisor.back_off(current_key),
        }
    }
    if let (Some(candidate), Ok(current_results)) = (routes.candidate, current.as_ref()) {
        let candidate_source = player_source;
        let candidate_inputs = inputs;
        let candidate_player_key = player_key.clone();
        let current_results = current_results.clone();
        tokio::spawn(async move {
            let result = solve_with_js_worker_lane(
                candidate.lane,
                &candidate_player_key,
                candidate_source.as_ref(),
                &candidate_inputs,
                YOUTUBE_JS_WORKER_CANDIDATE_TIMEOUT,
            )
            .await;
            let preserves_current = match &result {
                Ok(candidate) => {
                    js_worker_candidate_preserves_current_successes(candidate, &current_results)
                }
                Err(_) => false,
            };
            if !preserves_current {
                reject_js_worker_candidate(&candidate.proof).await;
                if let Err(error) = result {
                    tracing::debug!(
                        extractor = "native_js",
                        error = %error,
                        "YouTube candidate JavaScript worker shadow failed"
                    );
                }
            }
        });
    }
    current
}

impl JsWorkerSupervisor {
    fn routes(
        &mut self,
        player_key: &str,
        enforce_current_backoff: bool,
    ) -> Result<JsWorkerRoutes, String> {
        let JsWorkerSelection { current, candidate } = youtube_js_worker_selection()?;
        let now = Instant::now();
        self.failure_until.retain(|_, deadline| *deadline > now);
        self.rejected_candidates
            .retain(|_, deadline| *deadline > now);
        let current_lane = self.reconcile_current(current.clone());
        let current_key = WorkerPlayerKey {
            executable_identity: current.identity.clone(),
            player_key: player_key.to_owned(),
        };
        if enforce_current_backoff && self.failure_until.contains_key(&current_key) {
            return Err("Player JavaScript solver is temporarily backed off".to_owned());
        }
        let candidate = candidate.and_then(|candidate| {
            let key = WorkerPlayerKey {
                executable_identity: candidate.executable.identity.clone(),
                player_key: player_key.to_owned(),
            };
            if js_worker_candidate_is_rejected(&self.rejected_candidates, &key, now) {
                return None;
            }
            let candidate_changed = self
                .candidate
                .as_ref()
                .is_none_or(|lane| lane.executable.identity != candidate.executable.identity);
            if candidate_changed {
                self.candidate = Some(Arc::new(JsWorkerLane::new(candidate.executable.clone())));
            }
            Some(JsWorkerCandidateRoute {
                lane: self
                    .candidate
                    .as_ref()
                    .expect("the candidate worker lane was reconciled")
                    .clone(),
                proof: JsWorkerCandidateProof {
                    release_id: candidate.release_id,
                    ack_path: candidate.ack_path,
                    executable_identity: candidate.executable.identity,
                    baseline_current_identity: current.identity.clone(),
                    player_key: player_key.to_owned(),
                },
            })
        });
        if candidate.is_none() {
            self.candidate = None;
        }
        Ok(JsWorkerRoutes {
            current: current_lane,
            candidate,
        })
    }

    fn reconcile_current(&mut self, current: JsWorkerExecutable) -> Arc<JsWorkerLane> {
        let current_changed = self
            .current
            .as_ref()
            .is_none_or(|lane| lane.executable.identity != current.identity);
        if current_changed {
            self.current = Some(Arc::new(JsWorkerLane::new(current)));
            self.failure_until.clear();
        }
        self.current
            .as_ref()
            .expect("the current worker lane was reconciled")
            .clone()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    async fn stop(&mut self) {
        let lanes = [self.current.take(), self.candidate.take()];
        for lane in lanes.into_iter().flatten() {
            let mut process = lane.process.lock().await;
            if let Some(process) = process.take() {
                stop_js_worker_process(process).await;
            }
        }
    }

    fn back_off(&mut self, key: WorkerPlayerKey) {
        if self.failure_until.len() >= 16 {
            self.failure_until.clear();
        }
        self.failure_until
            .insert(key, Instant::now() + YOUTUBE_JS_WORKER_FAILURE_BACKOFF);
    }

    fn reject_candidate(&mut self, proof: &JsWorkerCandidateProof) {
        if self.rejected_candidates.len() >= 64 {
            self.rejected_candidates.clear();
        }
        self.rejected_candidates.insert(
            WorkerPlayerKey {
                executable_identity: proof.executable_identity.clone(),
                player_key: proof.player_key.clone(),
            },
            Instant::now() + YOUTUBE_JS_WORKER_CANDIDATE_REJECTION_TTL,
        );
    }
}

impl JsWorkerLane {
    fn new(executable: JsWorkerExecutable) -> Self {
        Self {
            executable,
            process: Arc::new(AsyncMutex::new(None)),
        }
    }
}

impl JsWorkerProcessLease {
    fn new(guard: OwnedMutexGuard<Option<JsWorkerProcess>>, process: JsWorkerProcess) -> Self {
        Self {
            guard,
            process: Some(process),
        }
    }

    fn process_mut(&mut self) -> &mut JsWorkerProcess {
        self.process
            .as_mut()
            .expect("the worker process lease is active")
    }

    fn commit(mut self) {
        *self.guard = self.process.take();
    }
}

impl Drop for JsWorkerProcessLease {
    fn drop(&mut self) {
        if let Some(mut process) = self.process.take() {
            let _ = process.child.start_kill();
        }
    }
}

async fn solve_with_js_worker_lane(
    lane: Arc<JsWorkerLane>,
    player_key: &str,
    player_source: &str,
    inputs: &[ChallengeInput],
    execution_timeout: Duration,
) -> Result<Vec<Result<ChallengeOutput, String>>, String> {
    let mut guard = tokio::time::timeout(
        YOUTUBE_JS_WORKER_QUEUE_TIMEOUT,
        lane.process.clone().lock_owned(),
    )
    .await
    .map_err(|_| "Player JavaScript solver queue timed out".to_owned())?;
    let process = match guard.take() {
        Some(process) => process,
        None => start_youtube_js_worker(lane.executable.clone())?,
    };
    let mut lease = JsWorkerProcessLease::new(guard, process);
    let result = tokio::time::timeout(
        execution_timeout,
        solve_with_js_worker(lease.process_mut(), player_key, player_source, inputs),
    )
    .await
    .map_err(|_| "Player JavaScript solver execution timed out".to_owned())?;
    match result {
        Ok(outputs) => {
            lease.commit();
            Ok(outputs)
        }
        Err(error) => Err(error),
    }
}

async fn reject_js_worker_candidate(proof: &JsWorkerCandidateProof) {
    let worker = YOUTUBE_JS_WORKER.get_or_init(|| AsyncMutex::new(JsWorkerSupervisor::default()));
    worker.lock().await.reject_candidate(proof);
}

fn js_worker_candidate_is_rejected(
    rejected: &HashMap<WorkerPlayerKey, Instant>,
    key: &WorkerPlayerKey,
    now: Instant,
) -> bool {
    rejected.get(key).is_some_and(|deadline| *deadline > now)
}

async fn run_youtube_js_candidate_solver_isolated(
    player_url: &str,
    player_source: Arc<str>,
    inputs: Vec<ChallengeInput>,
) -> Result<Option<JsWorkerCandidateBatch>, String> {
    let player_key = youtube_player_key(player_url, player_source.as_ref());
    let worker = YOUTUBE_JS_WORKER.get_or_init(|| AsyncMutex::new(JsWorkerSupervisor::default()));
    let candidate = {
        let mut supervisor = tokio::time::timeout(YOUTUBE_JS_WORKER_QUEUE_TIMEOUT, worker.lock())
            .await
            .map_err(|_| "Player JavaScript solver queue timed out".to_owned())?;
        supervisor.routes(&player_key, false)?.candidate
    };
    let Some(candidate) = candidate else {
        return Ok(None);
    };
    let outputs = solve_with_js_worker_lane(
        candidate.lane,
        &player_key,
        player_source.as_ref(),
        &inputs,
        YOUTUBE_JS_WORKER_CANDIDATE_TIMEOUT,
    )
    .await;
    match outputs {
        Ok(outputs) if outputs.iter().any(Result::is_ok) => Ok(Some(JsWorkerCandidateBatch {
            outputs,
            proof: candidate.proof,
        })),
        Ok(_) => {
            reject_js_worker_candidate(&candidate.proof).await;
            Err("updated Player JavaScript worker solved no challenges".to_owned())
        }
        Err(error) => {
            reject_js_worker_candidate(&candidate.proof).await;
            Err(error)
        }
    }
}

async fn solve_with_js_worker(
    process: &mut JsWorkerProcess,
    player_key: &str,
    player_source: &str,
    inputs: &[ChallengeInput],
) -> Result<Vec<Result<ChallengeOutput, String>>, String> {
    if process
        .child
        .try_wait()
        .map_err(|_| "Player JavaScript solver status check failed".to_owned())?
        .is_some()
    {
        return Err("Player JavaScript solver worker exited".to_owned());
    }
    let is_new_player = !process.loaded_players.contains(player_key);
    if is_new_player && process.loaded_players.len() >= YOUTUBE_JS_WORKER_SESSION_LIMIT {
        process.loaded_players.clear();
    }
    let request_id = process.next_request_id;
    process.next_request_id = process.next_request_id.wrapping_add(1).max(1);
    let request = serde_json::to_vec(&JsWorkerRequest {
        protocol_version: YOUTUBE_JS_WORKER_PROTOCOL_VERSION,
        request_id,
        player_key,
        player_source: is_new_player.then_some(player_source),
        inputs,
        per_input_results: true,
    })
    .map_err(|_| "Player JavaScript solver request encoding failed".to_owned())?;
    if request.is_empty() || request.len() > YOUTUBE_JS_WORKER_REQUEST_MAX_BYTES {
        return Err("Player JavaScript solver request exceeded its size limit".to_owned());
    }
    process
        .stdin
        .write_all(&(request.len() as u32).to_be_bytes())
        .await
        .map_err(|_| "Player JavaScript solver request write failed".to_owned())?;
    process
        .stdin
        .write_all(&request)
        .await
        .map_err(|_| "Player JavaScript solver request write failed".to_owned())?;
    process
        .stdin
        .flush()
        .await
        .map_err(|_| "Player JavaScript solver request flush failed".to_owned())?;

    let mut header = [0_u8; 4];
    process
        .stdout
        .read_exact(&mut header)
        .await
        .map_err(|_| "Player JavaScript solver response header failed".to_owned())?;
    let response_length = u32::from_be_bytes(header) as usize;
    if response_length == 0 || response_length > YOUTUBE_JS_WORKER_RESPONSE_MAX_BYTES {
        return Err("Player JavaScript solver response exceeded its size limit".to_owned());
    }
    let mut response = vec![0_u8; response_length];
    process
        .stdout
        .read_exact(&mut response)
        .await
        .map_err(|_| "Player JavaScript solver response read failed".to_owned())?;
    let response: JsWorkerResponse = serde_json::from_slice(&response)
        .map_err(|_| "Player JavaScript solver returned invalid JSON".to_owned())?;
    if response.protocol_version != YOUTUBE_JS_WORKER_PROTOCOL_VERSION {
        return Err("Player JavaScript solver returned an unsupported protocol".to_owned());
    }
    validate_js_worker_response_request_id(response.request_id, request_id)?;
    if let Some(error) = response.error {
        return Err(format!(
            "Player JavaScript solver rejected the challenge: {}",
            sanitize_js_worker_error(&error)
        ));
    }
    let results = if let Some(results) = response.results {
        validate_js_worker_results(&results, inputs.len())?
    } else {
        let outputs = response
            .outputs
            .ok_or_else(|| "Player JavaScript solver returned no outputs".to_owned())?;
        validate_js_worker_outputs(&outputs, inputs.len())?;
        outputs.into_iter().map(Ok).collect()
    };
    if is_new_player {
        process.loaded_players.insert(player_key.to_owned());
    }
    Ok(results)
}

fn validate_js_worker_response_request_id(
    response_id: Option<u64>,
    request_id: u64,
) -> Result<(), String> {
    if response_id.is_some_and(|response_id| response_id != request_id) {
        Err("Player JavaScript solver returned a mismatched request ID".to_owned())
    } else {
        Ok(())
    }
}

#[cfg_attr(not(test), allow(dead_code))]
async fn stop_js_worker_process(mut process: JsWorkerProcess) {
    let _ = process.child.start_kill();
    let _ = tokio::time::timeout(Duration::from_secs(2), process.child.wait()).await;
}

fn js_worker_candidate_preserves_current_successes(
    candidate: &[Result<ChallengeOutput, String>],
    current: &[Result<ChallengeOutput, String>],
) -> bool {
    candidate.len() == current.len()
        && candidate.iter().any(Result::is_ok)
        && candidate
            .iter()
            .zip(current)
            .all(|(candidate, current)| match (candidate, current) {
                (Err(_), Ok(_)) => false,
                _ => true,
            })
}

fn start_youtube_js_worker(executable: JsWorkerExecutable) -> Result<JsWorkerProcess, String> {
    let path = executable.path;
    let app_worker_mode = executable.app_worker_mode;
    let expected_identity = executable.identity;
    let mut command = Command::new(&path);
    if app_worker_mode {
        command.arg("--youtube-js-worker");
    }
    let mut child = command
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| "failed to start Player JavaScript solver worker".to_owned())?;
    let observed_identity = js_worker_executable(path, app_worker_mode)?.identity;
    if observed_identity != expected_identity {
        let _ = child.start_kill();
        return Err("Player JavaScript solver changed during startup".to_owned());
    }
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "Player JavaScript solver stdin was unavailable".to_owned())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Player JavaScript solver stdout was unavailable".to_owned())?;
    Ok(JsWorkerProcess {
        child,
        stdin,
        stdout,
        loaded_players: HashSet::new(),
        next_request_id: 1,
    })
}

fn youtube_js_worker_selection() -> Result<JsWorkerSelection, String> {
    if let Some(root) = env::var_os("WOTOHA_YOUTUBE_JS_WORKER_DIR").map(PathBuf::from) {
        return content_addressed_js_worker_selection(root);
    }
    if let Some(path) = env::var_os("WOTOHA_YOUTUBE_JS_WORKER").map(PathBuf::from) {
        if !path.is_absolute() {
            return Err("WOTOHA_YOUTUBE_JS_WORKER must be an absolute path".to_owned());
        }
        return current_js_worker_selection(path, false);
    }
    let executable =
        env::current_exe().map_err(|_| "current executable path was unavailable".to_owned())?;
    let parent = executable
        .parent()
        .ok_or_else(|| "current executable directory was unavailable".to_owned())?;
    let worker_name = if cfg!(windows) {
        "wotoha-youtube-js-worker.exe"
    } else {
        "wotoha-youtube-js-worker"
    };
    let sibling = parent.join(worker_name);
    if sibling.is_file() {
        return current_js_worker_selection(sibling, false);
    }
    if parent.file_name().is_some_and(|name| name == "deps")
        && let Some(debug_root) = parent.parent()
    {
        let debug_worker = debug_root.join(worker_name);
        if debug_worker.is_file() {
            return current_js_worker_selection(debug_worker, false);
        }
    }
    if executable
        .file_stem()
        .is_some_and(|name| name == "wotoha-app")
    {
        return current_js_worker_selection(executable, true);
    }
    Err("Player JavaScript solver worker executable was not found".to_owned())
}

fn current_js_worker_selection(
    path: PathBuf,
    app_worker_mode: bool,
) -> Result<JsWorkerSelection, String> {
    Ok(JsWorkerSelection {
        current: js_worker_executable(path, app_worker_mode)?,
        candidate: None,
    })
}

fn content_addressed_js_worker_selection(root: PathBuf) -> Result<JsWorkerSelection, String> {
    if !root.is_absolute() {
        return Err("WOTOHA_YOUTUBE_JS_WORKER_DIR must be an absolute path".to_owned());
    }
    let current_id = read_js_worker_pointer(&root.join("current"))?
        .ok_or_else(|| "YouTube JavaScript worker current pointer was missing".to_owned())?;
    let current = content_addressed_js_worker(&root, &current_id)?;
    let candidate_id = match read_js_worker_pointer(&root.join("candidate")) {
        Ok(candidate_id) => candidate_id.filter(|candidate_id| candidate_id != &current_id),
        Err(error) => {
            tracing::warn!(
                extractor = "native_js",
                error = %error,
                "YouTube ignored an invalid JavaScript worker candidate pointer"
            );
            None
        }
    };
    let candidate = candidate_id.and_then(|release_id| {
        let ack_path = env::var_os("WOTOHA_YOUTUBE_JS_WORKER_ACK")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/var/lib/wotoha/youtube-worker-ack"));
        if !ack_path.is_absolute() {
            tracing::warn!(
                extractor = "native_js",
                "YouTube ignored a JavaScript worker candidate with a relative ACK path"
            );
            return None;
        }
        match content_addressed_js_worker(&root, &release_id) {
            Ok(executable) => Some(JsWorkerCandidate {
                executable,
                release_id,
                ack_path,
            }),
            Err(error) => {
                tracing::warn!(
                    extractor = "native_js",
                    error = %error,
                    "YouTube ignored an invalid JavaScript worker candidate release"
                );
                None
            }
        }
    });
    Ok(JsWorkerSelection { current, candidate })
}

fn read_js_worker_pointer(path: &std::path::Path) -> Result<Option<String>, String> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("YouTube JavaScript worker pointer was unreadable".to_owned()),
    };
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > 128 {
        return Err("YouTube JavaScript worker pointer was invalid".to_owned());
    }
    let release_id = fs::read_to_string(path)
        .map_err(|_| "YouTube JavaScript worker pointer was unreadable".to_owned())?;
    let release_id = release_id.trim();
    if release_id.len() != 64
        || !release_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err("YouTube JavaScript worker pointer digest was invalid".to_owned());
    }
    Ok(Some(release_id.to_owned()))
}

fn content_addressed_js_worker(
    root: &std::path::Path,
    release_id: &str,
) -> Result<JsWorkerExecutable, String> {
    let worker_name = if cfg!(windows) {
        "wotoha-youtube-js-worker.exe"
    } else {
        "wotoha-youtube-js-worker"
    };
    let versions = fs::canonicalize(root.join("versions"))
        .map_err(|_| "YouTube JavaScript worker versions directory was unavailable".to_owned())?;
    let requested = versions.join(release_id).join(worker_name);
    if fs::symlink_metadata(&requested)
        .map_err(|_| "YouTube JavaScript worker release was unavailable".to_owned())?
        .file_type()
        .is_symlink()
    {
        return Err("YouTube JavaScript worker release must not be a symlink".to_owned());
    }
    let path = fs::canonicalize(&requested)
        .map_err(|_| "YouTube JavaScript worker release was unavailable".to_owned())?;
    if !path.starts_with(&versions) {
        return Err("YouTube JavaScript worker release escaped its versions directory".to_owned());
    }
    let executable = js_worker_executable(path, false)?;
    let cache_key = format!("{release_id}:{}", executable.identity);
    let verified = YOUTUBE_JS_WORKER_VERIFIED_DIGESTS.get_or_init(|| StdMutex::new(HashSet::new()));
    let mut verified = verified
        .lock()
        .map_err(|_| "YouTube JavaScript worker digest cache was unavailable".to_owned())?;
    if !verified.contains(&cache_key) {
        use std::io::Read;

        let mut file = fs::File::open(&executable.path)
            .map_err(|_| "YouTube JavaScript worker release was unreadable".to_owned())?;
        let mut digest = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = file
                .read(&mut buffer)
                .map_err(|_| "YouTube JavaScript worker release was unreadable".to_owned())?;
            if read == 0 {
                break;
            }
            digest.update(&buffer[..read]);
        }
        let actual = digest
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        if actual != release_id {
            return Err("YouTube JavaScript worker digest did not match its release ID".to_owned());
        }
        if verified.len() >= 16 {
            verified.clear();
        }
        verified.insert(cache_key);
    }
    Ok(executable)
}

fn acknowledge_validated_js_worker_candidate(proof: &JsWorkerCandidateProof) {
    let selection = match youtube_js_worker_selection() {
        Ok(selection) => selection,
        Err(error) => {
            tracing::warn!(
                extractor = "native_js",
                error = %error,
                "YouTube could not revalidate the JavaScript worker candidate before ACK"
            );
            return;
        }
    };
    match write_validated_js_worker_candidate_ack(&selection, proof) {
        Ok(true) => {
            tracing::info!(
                extractor = "native_js",
                "YouTube acknowledged a JavaScript worker after stream validation"
            );
        }
        Ok(false) => {
            tracing::debug!(
                extractor = "native_js",
                "YouTube skipped ACK for a stale JavaScript worker candidate proof"
            );
        }
        Err(error) => {
            tracing::warn!(
                extractor = "native_js",
                error = %error,
                "YouTube could not acknowledge the validated JavaScript worker candidate"
            );
        }
    }
}

fn write_validated_js_worker_candidate_ack(
    selection: &JsWorkerSelection,
    proof: &JsWorkerCandidateProof,
) -> Result<bool, String> {
    if selection.current.identity == proof.executable_identity {
        return Ok(false);
    }
    let still_pending = selection.candidate.as_ref().is_some_and(|candidate| {
        candidate.release_id == proof.release_id
            && candidate.executable.identity == proof.executable_identity
            && candidate.ack_path == proof.ack_path
            && selection.current.identity == proof.baseline_current_identity
    });
    if !still_pending {
        return Ok(false);
    }
    write_js_worker_ack(&proof.ack_path, &proof.release_id)?;
    Ok(true)
}

fn write_js_worker_ack(path: &std::path::Path, release_id: &str) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "YouTube JavaScript worker ACK directory was unavailable".to_owned())?;
    let temporary = parent.join(format!(
        ".youtube-worker-ack-{}-{}.tmp",
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let result = (|| {
        use std::io::Write;

        let mut file = fs::File::create(&temporary)
            .map_err(|_| "YouTube JavaScript worker ACK write failed".to_owned())?;
        file.write_all(format!("{release_id}\n").as_bytes())
            .map_err(|_| "YouTube JavaScript worker ACK write failed".to_owned())?;
        file.sync_all()
            .map_err(|_| "YouTube JavaScript worker ACK sync failed".to_owned())?;
        drop(file);
        fs::rename(&temporary, path)
            .map_err(|_| "YouTube JavaScript worker ACK promotion failed".to_owned())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn js_worker_executable(
    path: PathBuf,
    app_worker_mode: bool,
) -> Result<JsWorkerExecutable, String> {
    let metadata = fs::metadata(&path)
        .map_err(|_| "Player JavaScript solver worker metadata was unavailable".to_owned())?;
    if !metadata.is_file() {
        return Err("Player JavaScript solver worker was not a regular file".to_owned());
    }
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt;
        format!(
            "{}:{}:{}:{}:{}:{}:{}",
            path.display(),
            metadata.dev(),
            metadata.ino(),
            metadata.len(),
            metadata.mtime_nsec(),
            metadata.ctime(),
            metadata.ctime_nsec()
        )
    };
    #[cfg(not(unix))]
    let identity = format!(
        "{}:{}:{}",
        path.display(),
        metadata.len(),
        metadata
            .modified()
            .ok()
            .and_then(|modified| modified.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos())
            .unwrap_or_default()
    );
    Ok(JsWorkerExecutable {
        path,
        app_worker_mode,
        identity,
    })
}

fn youtube_player_key(player_url: &str, player_source: &str) -> String {
    let digest = Sha256::digest(player_source.as_bytes());
    let mut key = String::with_capacity(player_url.len() + 65);
    key.push_str(player_url);
    key.push('#');
    for byte in digest {
        let _ = write!(key, "{byte:02x}");
    }
    key
}

fn validate_js_worker_outputs(
    outputs: &[ChallengeOutput],
    expected_count: usize,
) -> Result<(), String> {
    if outputs.len() != expected_count
        || outputs.len() > YOUTUBE_CHALLENGE_JOB_LIMIT
        || outputs.iter().any(|output| {
            output
                .signature
                .as_ref()
                .is_some_and(|value| value.len() > YOUTUBE_CHALLENGE_VALUE_MAX_BYTES)
                || output
                    .n
                    .as_ref()
                    .is_some_and(|value| value.len() > YOUTUBE_CHALLENGE_VALUE_MAX_BYTES)
        })
    {
        return Err("Player JavaScript solver returned invalid outputs".to_owned());
    }
    Ok(())
}

fn validate_js_worker_results(
    results: &[JsWorkerChallengeResult],
    expected_count: usize,
) -> Result<Vec<Result<ChallengeOutput, String>>, String> {
    if results.len() != expected_count || results.len() > YOUTUBE_CHALLENGE_JOB_LIMIT {
        return Err("Player JavaScript solver returned invalid result count".to_owned());
    }
    results
        .iter()
        .map(|result| match (&result.output, &result.error) {
            (Some(output), None) => {
                validate_js_worker_outputs(std::slice::from_ref(output), 1)?;
                Ok(Ok(output.clone()))
            }
            (None, Some(error)) if !error.is_empty() && error.len() <= 256 => Ok(Err(format!(
                "Player JavaScript challenge had no unique solution: {}",
                sanitize_js_worker_error(error)
            ))),
            _ => Err("Player JavaScript solver returned an invalid item result".to_owned()),
        })
        .collect()
}

fn sanitize_js_worker_error(error: &str) -> String {
    error
        .chars()
        .filter(|character| !character.is_control())
        .take(256)
        .collect()
}

async fn solve_android_cipher_formats(
    probe_client: &Client,
    response: &mut AndroidPlayerResponse,
    player_url: &str,
) -> Result<usize, String> {
    Ok(solve_android_cipher_formats_with_worker(
        probe_client,
        response,
        player_url,
        CipherWorkerChoice::Current,
    )
    .await?
    .solved)
}

async fn solve_android_cipher_formats_with_worker(
    _probe_client: &Client,
    response: &mut AndroidPlayerResponse,
    player_url: &str,
    worker_choice: CipherWorkerChoice,
) -> Result<SolvedCipherFormats, String> {
    let Some(streaming_data) = response.streaming_data.as_mut() else {
        return Ok(SolvedCipherFormats {
            solved: 0,
            candidate_proof: None,
        });
    };
    let mut jobs = Vec::new();
    for (format_index, format) in streaming_data.adaptive_formats.iter().enumerate() {
        if !format.mime_type.starts_with("audio/") || format.url.is_some() {
            continue;
        }
        let Some(cipher) = format.signature_cipher.as_deref() else {
            continue;
        };
        match cipher_format_job(format_index, cipher) {
            Ok(Some(job)) => jobs.push(job),
            Ok(None) => {}
            Err(error) => {
                tracing::debug!(
                    extractor = "native_js",
                    format_index,
                    error = %error,
                    "YouTube skipped a malformed cipher format"
                );
            }
        }
    }
    if jobs.is_empty() {
        return Ok(SolvedCipherFormats {
            solved: 0,
            candidate_proof: None,
        });
    }
    if jobs.len() > YOUTUBE_CHALLENGE_JOB_LIMIT {
        return Err("YouTube returned too many Player JavaScript challenge jobs".to_owned());
    }

    let player_source = youtube_player_source(player_url).await?;
    let mut inputs = Vec::with_capacity(jobs.len() * 2);
    let mut targets = Vec::with_capacity(jobs.len() * 2);
    for (job_index, job) in jobs.iter().enumerate() {
        if let Some(signature) = job.input.signature.as_ref() {
            inputs.push(ChallengeInput {
                signature: Some(signature.clone()),
                n: None,
            });
            targets.push(CipherOutputTarget::Signature(job_index));
        }
        if let Some(n) = job.input.n.as_ref() {
            inputs.push(ChallengeInput {
                signature: None,
                n: Some(n.clone()),
            });
            targets.push(CipherOutputTarget::N(job_index));
        }
    }
    if inputs.len() > YOUTUBE_CHALLENGE_JOB_LIMIT {
        return Err("YouTube returned too many Player JavaScript challenge inputs".to_owned());
    }
    let (outputs, candidate_proof) = match worker_choice {
        CipherWorkerChoice::Current => (
            run_youtube_js_solver_isolated(player_url, player_source, inputs).await?,
            None,
        ),
        CipherWorkerChoice::Candidate => {
            let Some(candidate) =
                run_youtube_js_candidate_solver_isolated(player_url, player_source, inputs).await?
            else {
                return Err("updated Player JavaScript worker candidate was unavailable".to_owned());
            };
            (candidate.outputs, Some(candidate.proof))
        }
    };
    if outputs.len() != targets.len() {
        return Err("Player JavaScript solver returned the wrong output count".to_owned());
    }

    let solved_urls = apply_cipher_outputs(&jobs, targets, outputs)?;
    for (job, stream_url) in jobs.into_iter().zip(solved_urls) {
        let stream_url = match stream_url {
            Ok(stream_url) => stream_url,
            Err(error) => {
                tracing::debug!(
                    extractor = "native_js",
                    format_index = job.format_index,
                    error = %error,
                    "YouTube skipped a cipher format with an unsolved challenge"
                );
                continue;
            }
        };
        let Some(format) = streaming_data.adaptive_formats.get_mut(job.format_index) else {
            return Err("Player JavaScript format index changed during solving".to_owned());
        };
        format.url = Some(stream_url);
    }
    let solved = streaming_data
        .adaptive_formats
        .iter()
        .filter(|format| {
            format.mime_type.starts_with("audio/")
                && format.url.is_some()
                && format.signature_cipher.is_some()
        })
        .count();
    Ok(SolvedCipherFormats {
        solved,
        candidate_proof,
    })
}

fn apply_cipher_outputs(
    jobs: &[CipherFormatJob],
    targets: Vec<CipherOutputTarget>,
    outputs: Vec<Result<ChallengeOutput, String>>,
) -> Result<Vec<Result<String, String>>, String> {
    if targets.len() != outputs.len() {
        return Err("Player JavaScript solver returned the wrong output count".to_owned());
    }
    let mut solved_urls = jobs
        .iter()
        .map(|job| Ok(job.stream_url.clone()))
        .collect::<Vec<_>>();
    for (target, output) in targets.into_iter().zip(outputs) {
        let job_index = match target {
            CipherOutputTarget::Signature(job_index) | CipherOutputTarget::N(job_index) => {
                job_index
            }
        };
        if job_index >= jobs.len() {
            return Err("Player JavaScript solver returned an invalid cipher target".to_owned());
        }
        let output = match output {
            Ok(output) => output,
            Err(error) => {
                solved_urls[job_index] = Err(error);
                continue;
            }
        };
        if solved_urls[job_index].is_err() {
            continue;
        }
        let (key, value) = match target {
            CipherOutputTarget::Signature(job_index) => {
                let Some(value) = output.signature.filter(|signature| {
                    !signature.is_empty() && signature.len() <= YOUTUBE_CHALLENGE_VALUE_MAX_BYTES
                }) else {
                    solved_urls[job_index] =
                        Err("Player JavaScript returned an empty signature".to_owned());
                    continue;
                };
                (jobs[job_index].signature_parameter.as_str(), value)
            }
            CipherOutputTarget::N(_) => {
                let Some(value) = output
                    .n
                    .filter(|n| !n.is_empty() && n.len() <= YOUTUBE_CHALLENGE_VALUE_MAX_BYTES)
                else {
                    solved_urls[job_index] =
                        Err("Player JavaScript returned an empty n value".to_owned());
                    continue;
                };
                ("n", value)
            }
        };
        let current_url = solved_urls[job_index]
            .as_ref()
            .expect("a failed cipher job was skipped");
        solved_urls[job_index] = url_with_query_value(current_url, key, &value);
    }
    Ok(solved_urls)
}

async fn solve_url_n_challenge(
    _probe_client: &Client,
    stream_url: &str,
    player_url: Option<&str>,
    challenge_detected: Option<&AtomicBool>,
) -> Result<Option<SolvedNChallenge>, String> {
    let Some(player_url) = player_url else {
        return Ok(None);
    };
    let Some((n, in_path)) = url_n_challenge_input(stream_url)? else {
        return Ok(None);
    };
    if let Some(challenge_detected) = challenge_detected {
        challenge_detected.store(true, Ordering::Relaxed);
    }
    let player_source = youtube_player_source(player_url).await?;
    let input = ChallengeInput {
        signature: None,
        n: Some(n),
    };
    let (outputs, candidate_proof) =
        match run_youtube_js_solver(player_url, player_source.clone(), vec![input.clone()]).await {
            Ok(outputs) => (outputs, None),
            Err(current_error) => {
                let Some(candidate) = run_youtube_js_candidate_solver_isolated(
                    player_url,
                    player_source,
                    vec![input],
                )
                .await?
                else {
                    return Err(current_error);
                };
                (
                    candidate
                        .outputs
                        .into_iter()
                        .collect::<Result<Vec<_>, _>>()?,
                    Some(candidate.proof),
                )
            }
        };
    solved_n_challenge(stream_url, in_path, outputs, candidate_proof).map(Some)
}

async fn solve_url_n_challenge_candidate(
    stream_url: &str,
    player_url: Option<&str>,
) -> Result<Option<SolvedNChallenge>, String> {
    let Some(player_url) = player_url else {
        return Ok(None);
    };
    let Some((n, in_path)) = url_n_challenge_input(stream_url)? else {
        return Ok(None);
    };
    let player_source = youtube_player_source(player_url).await?;
    let Some(candidate) = run_youtube_js_candidate_solver_isolated(
        player_url,
        player_source,
        vec![ChallengeInput {
            signature: None,
            n: Some(n),
        }],
    )
    .await?
    else {
        return Ok(None);
    };
    let outputs = candidate
        .outputs
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;
    solved_n_challenge(stream_url, in_path, outputs, Some(candidate.proof)).map(Some)
}

fn spawn_n_candidate_format_validation(
    probe_client: Client,
    original_url: String,
    player_url: String,
    mut format: ChosenFormat,
) {
    tokio::spawn(async move {
        let Ok(Some(candidate)) =
            solve_url_n_challenge_candidate(&original_url, Some(&player_url)).await
        else {
            return;
        };
        format.stream_url = candidate.stream_url;
        if let Some(proof) = candidate.candidate_proof.as_ref() {
            if validate_chosen_format(&probe_client, &format).await.is_ok() {
                acknowledge_validated_js_worker_candidate(proof);
            } else {
                reject_js_worker_candidate(proof).await;
            }
        }
    });
}

fn spawn_n_candidate_request_validation(
    probe_client: Client,
    original_url: String,
    player_url: String,
    mut request: TrackRequest,
) {
    tokio::spawn(async move {
        let Ok(Some(candidate)) =
            solve_url_n_challenge_candidate(&original_url, Some(&player_url)).await
        else {
            return;
        };
        match &mut request.prepared {
            PreparedSource::Http { stream_url, .. } => {
                *stream_url = candidate.stream_url.clone().into()
            }
            PreparedSource::Hls { playlist_url, .. } => {
                *playlist_url = candidate.stream_url.clone().into()
            }
        }
        if let Some(proof) = candidate.candidate_proof.as_ref() {
            if validate_prepared_source(&probe_client, &request.prepared)
                .await
                .is_ok()
            {
                acknowledge_validated_js_worker_candidate(proof);
            } else {
                reject_js_worker_candidate(proof).await;
            }
        }
    });
}

fn url_n_challenge_input(stream_url: &str) -> Result<Option<(String, bool)>, String> {
    let parsed = Url::parse(stream_url)
        .map_err(|error| format!("invalid stream URL for n solve: {error}"))?;
    if !youtube_googlevideo_origin_is_allowed(&parsed) {
        return Err("YouTube n challenge URL used an unexpected origin".to_owned());
    }
    let query_n = parsed
        .query_pairs()
        .find(|(key, _)| key == "n")
        .map(|(_, value)| value.into_owned())
        .filter(|value| !value.is_empty());
    let path_n = path_n_challenge(&parsed);
    let Some((n, in_path)) = query_n
        .map(|value| (value, false))
        .or_else(|| path_n.map(|value| (value, true)))
    else {
        return Ok(None);
    };
    if stream_url.len() > 64 * 1024 || n.len() > YOUTUBE_CHALLENGE_VALUE_MAX_BYTES {
        return Err("YouTube n challenge input exceeded its size limit".to_owned());
    }
    Ok(Some((n, in_path)))
}

fn solved_n_challenge(
    stream_url: &str,
    in_path: bool,
    outputs: Vec<ChallengeOutput>,
    candidate_proof: Option<JsWorkerCandidateProof>,
) -> Result<SolvedNChallenge, String> {
    let solved_n = outputs
        .into_iter()
        .next()
        .and_then(|output| output.n)
        .filter(|value| !value.is_empty() && value.len() <= YOUTUBE_CHALLENGE_VALUE_MAX_BYTES)
        .ok_or_else(|| "Player JavaScript returned an empty n value".to_owned())?;
    let stream_url = if in_path {
        url_with_path_n_value(stream_url, &solved_n)?
    } else {
        url_with_query_value(stream_url, "n", &solved_n)?
    };
    Ok(SolvedNChallenge {
        stream_url,
        candidate_proof,
    })
}

#[derive(Debug)]
struct CipherFormatJob {
    format_index: usize,
    stream_url: String,
    signature_parameter: String,
    input: ChallengeInput,
}

#[derive(Clone, Copy, Debug)]
enum CipherOutputTarget {
    Signature(usize),
    N(usize),
}

fn cipher_format_job(format_index: usize, cipher: &str) -> Result<Option<CipherFormatJob>, String> {
    if cipher.len() > 64 * 1024 {
        return Err("YouTube signatureCipher exceeded the 64 KiB limit".to_owned());
    }
    let mut stream_url = None;
    let mut signature = None;
    let mut signature_parameter = None;
    for (key, value) in url::form_urlencoded::parse(cipher.as_bytes()) {
        match key.as_ref() {
            "url" if stream_url.is_none() => stream_url = Some(value.into_owned()),
            "s" if signature.is_none() => signature = Some(value.into_owned()),
            "sp" if signature_parameter.is_none() => signature_parameter = Some(value.into_owned()),
            _ => {}
        }
    }
    let Some(stream_url) = stream_url else {
        return Ok(None);
    };
    if stream_url.len() > 64 * 1024 {
        return Err("YouTube ciphered stream URL exceeded the 64 KiB limit".to_owned());
    }
    let parsed =
        Url::parse(&stream_url).map_err(|error| format!("invalid ciphered stream URL: {error}"))?;
    if !youtube_googlevideo_url_is_allowed(&parsed) {
        return Err("YouTube ciphered stream URL used an unexpected origin".to_owned());
    }
    let n = parsed
        .query_pairs()
        .find(|(key, _)| key == "n")
        .map(|(_, value)| value.into_owned())
        .filter(|value| !value.is_empty());
    let signature = signature.filter(|value| !value.is_empty());
    if signature
        .as_ref()
        .is_some_and(|value| value.len() > YOUTUBE_CHALLENGE_VALUE_MAX_BYTES)
        || n.as_ref()
            .is_some_and(|value| value.len() > YOUTUBE_CHALLENGE_VALUE_MAX_BYTES)
    {
        return Err("YouTube challenge value exceeded the 16 KiB limit".to_owned());
    }
    if signature.is_none() && n.is_none() {
        return Ok(None);
    }
    let signature_parameter = signature_parameter.unwrap_or_else(|| "signature".to_owned());
    if signature_parameter.is_empty()
        || signature_parameter.len() > 64
        || !signature_parameter
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err("YouTube signature parameter name was invalid".to_owned());
    }
    Ok(Some(CipherFormatJob {
        format_index,
        stream_url,
        signature_parameter,
        input: ChallengeInput { signature, n },
    }))
}

async fn youtube_player_source(player_url: &str) -> Result<Arc<str>, String> {
    let url = Url::parse(player_url)
        .map_err(|error| format!("invalid Player JavaScript URL: {error}"))?;
    if !youtube_player_url_is_allowed(&url) {
        return Err("Player JavaScript URL used an unexpected origin or path".to_owned());
    }
    let cache = YOUTUBE_PLAYER_SCRIPT.get_or_init(|| AsyncMutex::new(None));
    let mut cached = cache.lock().await;
    if let Some(cached) = cached.as_ref()
        && cached.url == player_url
        && cached.cached_at.elapsed() <= YOUTUBE_PLAYER_SCRIPT_TTL
    {
        return Ok(cached.source.clone());
    }

    let player_client = YOUTUBE_PLAYER_CLIENT
        .get_or_init(|| {
            Client::builder()
                .https_only(true)
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(12))
                .redirect(Policy::custom(|attempt| {
                    if attempt.previous().len() >= 3 {
                        attempt.error("too many Player JavaScript redirects")
                    } else if youtube_player_url_is_allowed(attempt.url()) {
                        attempt.follow()
                    } else {
                        attempt.error("Player JavaScript redirect was rejected")
                    }
                }))
                .build()
                .map_err(|_| "failed to build Player JavaScript HTTP client".to_owned())
        })
        .as_ref()
        .map_err(Clone::clone)?;
    let mut response = player_client
        .get(url)
        .send()
        .await
        .map_err(|_| "Player JavaScript download failed".to_owned())?;
    if !response.status().is_success() {
        return Err(format!(
            "Player JavaScript returned HTTP {}",
            response.status()
        ));
    }
    if !youtube_player_url_is_allowed(response.url()) {
        return Err("Player JavaScript redirected to an unexpected URL".to_owned());
    }
    if let Some(content_type) = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
    {
        let content_type = content_type.to_ascii_lowercase();
        if !content_type.contains("javascript")
            && !content_type.starts_with("text/plain")
            && !content_type.starts_with("application/octet-stream")
        {
            return Err("Player JavaScript returned an unexpected content type".to_owned());
        }
    }
    if response
        .content_length()
        .is_some_and(|length| length > YOUTUBE_PLAYER_SCRIPT_MAX_BYTES as u64)
    {
        return Err("Player JavaScript exceeded the 8 MiB limit".to_owned());
    }
    let mut bytes = Vec::with_capacity(
        response
            .content_length()
            .unwrap_or_default()
            .min(YOUTUBE_PLAYER_SCRIPT_MAX_BYTES as u64) as usize,
    );
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "Player JavaScript download failed".to_owned())?
    {
        if bytes.len().saturating_add(chunk.len()) > YOUTUBE_PLAYER_SCRIPT_MAX_BYTES {
            return Err("Player JavaScript exceeded the 8 MiB limit".to_owned());
        }
        bytes.extend_from_slice(&chunk);
    }
    let source = Arc::<str>::from(
        String::from_utf8(bytes).map_err(|_| "Player JavaScript was not valid UTF-8".to_owned())?,
    );
    if source.is_empty() {
        return Err("Player JavaScript response was empty".to_owned());
    }

    *cached = Some(CachedPlayerScript {
        url: player_url.to_owned(),
        source: source.clone(),
        cached_at: Instant::now(),
    });
    Ok(source)
}

async fn fetch_direct_native_player_response(
    probe_client: &Client,
    video_id: &str,
    profile: &NativeClientProfile,
    player_url: Option<&str>,
    challenge_detected: Option<&AtomicBool>,
) -> Result<Option<AndroidPlayerResponse>, ResolveError> {
    let response =
        send_native_player_request(probe_client, video_id, profile, None, None, None).await?;
    let response =
        solve_native_player_challenges(probe_client, response, player_url, challenge_detected)
            .await;
    if response.has_playable_stream() {
        return Ok(Some(response));
    }
    let Some((player_token, was_cached)) =
        po_token(profile, PoTokenContext::Player, video_id, None, false).await
    else {
        return Ok(Some(response));
    };
    let response = send_native_player_request(
        probe_client,
        video_id,
        profile,
        None,
        None,
        Some(player_token.value.as_str()),
    )
    .await?;
    let response =
        solve_native_player_challenges(probe_client, response, player_url, challenge_detected)
            .await;
    if response.has_playable_stream() {
        return Ok(Some(response));
    }
    invalidate_po_token(profile, PoTokenContext::Player, video_id, None);
    if !was_cached {
        return Ok(Some(response));
    }
    let Some((fresh_token, _)) =
        po_token(profile, PoTokenContext::Player, video_id, None, true).await
    else {
        return Ok(Some(response));
    };
    let fresh_response = send_native_player_request(
        probe_client,
        video_id,
        profile,
        None,
        None,
        Some(fresh_token.value.as_str()),
    )
    .await?;
    let fresh_response = solve_native_player_challenges(
        probe_client,
        fresh_response,
        player_url,
        challenge_detected,
    )
    .await;
    if !fresh_response.has_playable_stream() {
        invalidate_po_token(profile, PoTokenContext::Player, video_id, None);
    }
    Ok(Some(fresh_response))
}

fn chosen_format_from_android_streaming_data(
    streaming_data: &AndroidStreamingData,
    headers: Vec<PreparedHeader>,
) -> Option<ChosenFormat> {
    if let Some(format) = choose_android_audio_stream(&streaming_data.adaptive_formats) {
        let stream_url = format.url.as_ref()?;
        let content_length = format
            .content_length
            .clone()
            .or_else(|| parse_content_length_from_url(stream_url).map(|v| v.to_string()));
        return Some(ChosenFormat {
            stream_url: stream_url.clone(),
            content_length: content_length.clone(),
            is_hls: false,
            headers,
            range_chunk_size: content_length.is_some().then_some(YOUTUBE_RANGE_CHUNK_SIZE),
            po_token_expires_at_unix: None,
        });
    }

    if let Some(hls_manifest_url) = streaming_data
        .hls_manifest_url
        .as_ref()
        .filter(|url| !url.is_empty())
    {
        return Some(ChosenFormat {
            stream_url: hls_manifest_url.clone(),
            content_length: None,
            is_hls: true,
            headers,
            range_chunk_size: None,
            po_token_expires_at_unix: None,
        });
    }

    None
}

fn pick_android_thumbnail(thumbnails: &[AndroidThumbnail]) -> Option<Arc<str>> {
    thumbnails
        .iter()
        .filter(|thumbnail| !thumbnail.url.is_empty())
        .max_by_key(|thumbnail| {
            thumbnail.width.unwrap_or_default() * thumbnail.height.unwrap_or_default()
        })
        .map(|thumbnail| Arc::<str>::from(thumbnail.url.clone()))
}

fn youtube_video_id_from_url(raw_url: &str) -> Option<String> {
    let url = Url::parse(raw_url).ok()?;
    let host = url.host_str()?.to_ascii_lowercase();

    if host == "youtu.be" {
        return url
            .path_segments()?
            .find(|segment| !segment.is_empty())
            .map(str::to_owned);
    }

    if !matches!(
        host.as_str(),
        "youtube.com" | "www.youtube.com" | "m.youtube.com" | "music.youtube.com"
    ) {
        return None;
    }

    if let Some(video_id) = url
        .query_pairs()
        .find(|(key, _)| key == "v")
        .map(|(_, value)| value.to_string())
        .filter(|value| !value.is_empty())
    {
        return Some(video_id);
    }

    let mut segments = url.path_segments()?;
    match segments.next()? {
        "shorts" | "embed" | "live" => segments
            .next()
            .filter(|segment| !segment.is_empty())
            .map(str::to_owned),
        _ => None,
    }
}

fn choose_android_audio_stream(
    formats: &[AndroidAdaptiveFormat],
) -> Option<&AndroidAdaptiveFormat> {
    formats
        .iter()
        .filter(|format| {
            format.mime_type.starts_with("audio/")
                && format.url.as_ref().is_some_and(|url| !url.is_empty())
        })
        .max_by_key(|format| {
            (
                format.mime_type.contains("opus"),
                format.audio_bitrate.or(format.bitrate).unwrap_or_default(),
                format.bitrate.unwrap_or_default(),
            )
        })
}

fn native_api_headers(
    profile: &NativeClientProfile,
    visitor_data: Option<&str>,
) -> Result<HeaderMap, ResolveError> {
    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static("content-type"),
        HeaderValue::from_static("application/json"),
    );
    headers.insert(
        HeaderName::from_static("origin"),
        HeaderValue::from_static("https://www.youtube.com"),
    );
    headers.insert(
        HeaderName::from_static("referer"),
        HeaderValue::from_static("https://www.youtube.com/"),
    );
    headers.insert(
        HeaderName::from_static("user-agent"),
        HeaderValue::from_str(&profile.user_agent)
            .map_err(|_| ResolveError::InvalidHeaderValue(profile.user_agent.clone()))?,
    );
    headers.insert(
        HeaderName::from_static("x-youtube-client-name"),
        HeaderValue::from_str(&profile.client_number)
            .map_err(|_| ResolveError::InvalidHeaderValue(profile.client_number.clone()))?,
    );
    headers.insert(
        HeaderName::from_static("x-youtube-client-version"),
        HeaderValue::from_str(&profile.client_version)
            .map_err(|_| ResolveError::InvalidHeaderValue(profile.client_version.clone()))?,
    );
    if let Some(visitor_data) = visitor_data {
        headers.insert(
            HeaderName::from_static("x-goog-visitor-id"),
            HeaderValue::from_str(visitor_data)
                .map_err(|_| ResolveError::InvalidHeaderValue(visitor_data.to_owned()))?,
        );
    }

    Ok(headers)
}

fn native_player_request(
    video_id: &str,
    profile: &NativeClientProfile,
    signature_timestamp: Option<u64>,
    visitor_data: Option<&str>,
    player_token: Option<&str>,
) -> serde_json::Value {
    let mut client = serde_json::json!({
        "clientName": profile.client_name.as_str(),
        "clientVersion": profile.client_version.as_str(),
        "userAgent": profile.user_agent.as_str(),
        "osName": profile.os_name.as_str(),
        "osVersion": profile.os_version.as_str(),
        "hl": "en",
        "timeZone": "UTC",
        "utcOffsetMinutes": 0,
    });
    let client = client
        .as_object_mut()
        .expect("native YouTube client context should be an object");
    if let Some(device_make) = profile.device_make.as_deref() {
        client.insert("deviceMake".to_owned(), device_make.into());
    }
    if let Some(device_model) = profile.device_model.as_deref() {
        client.insert("deviceModel".to_owned(), device_model.into());
    }
    if let Some(android_sdk_version) = profile.android_sdk_version {
        client.insert("androidSdkVersion".to_owned(), android_sdk_version.into());
    }
    if let Some(visitor_data) = visitor_data {
        client.insert("visitorData".to_owned(), visitor_data.into());
    }

    let mut request = serde_json::json!({
        "context": { "client": client },
        "contentCheckOk": true,
        "racyCheckOk": true,
        "videoId": video_id,
    });
    if let Some(signature_timestamp) = signature_timestamp {
        request
            .as_object_mut()
            .expect("native YouTube player request should be an object")
            .insert(
                "playbackContext".to_owned(),
                serde_json::json!({
                    "contentPlaybackContext": {
                        "signatureTimestamp": signature_timestamp,
                        "html5Preference": "HTML5_PREF_WANTS",
                    }
                }),
            );
    }
    if let Some(player_token) = player_token {
        request
            .as_object_mut()
            .expect("native YouTube player request should be an object")
            .insert(
                "serviceIntegrityDimensions".to_owned(),
                serde_json::json!({ "poToken": player_token }),
            );
    }
    request
}

fn extract_signature_timestamp(html: &str) -> Option<u64> {
    let regex = Regex::new(r#""(?:sts|STS)"\s*:\s*(\d+)"#).ok()?;
    let captures = regex.captures(html)?;
    captures
        .get(1)
        .and_then(|capture| capture.as_str().parse::<u64>().ok())
}

fn extract_visitor_data(html: &str) -> Option<String> {
    let regex = Regex::new(r#""(?:VISITOR_DATA|visitorData)"\s*:\s*"([^"]+)""#).ok()?;
    let encoded = regex.captures(html)?.get(1)?.as_str();
    serde_json::from_str::<String>(&format!("\"{encoded}\""))
        .ok()
        .or_else(|| Some(encoded.to_owned()))
}

fn extract_player_url(html: &str) -> Option<String> {
    let json_pattern = Regex::new(r#""(?:jsUrl|PLAYER_JS_URL)"\s*:\s*"([^"]+)""#).ok()?;
    let script_pattern =
        Regex::new(r#"(?i)<script[^>]+src=["']([^"']*/s/player/[^"']+\.js[^"']*)["']"#).ok()?;
    let encoded = json_pattern
        .captures(html)
        .and_then(|captures| captures.get(1))
        .or_else(|| {
            script_pattern
                .captures(html)
                .and_then(|captures| captures.get(1))
        })?
        .as_str();
    let decoded = serde_json::from_str::<String>(&format!("\"{encoded}\""))
        .unwrap_or_else(|_| encoded.to_owned());
    let base = Url::parse("https://www.youtube.com").expect("static YouTube URL should parse");
    let url = Url::parse(&decoded).or_else(|_| base.join(&decoded)).ok()?;
    youtube_player_url_is_allowed(&url).then(|| url.to_string())
}

fn youtube_player_url_is_allowed(url: &Url) -> bool {
    url.as_str().len() <= 2048
        && url.scheme() == "https"
        && matches!(url.host_str(), Some("youtube.com" | "www.youtube.com"))
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none_or(|port| port == 443)
        && url.fragment().is_none()
        && url.path().starts_with("/s/player/")
        && url.path().ends_with(".js")
}

fn youtube_googlevideo_url_is_allowed(url: &Url) -> bool {
    youtube_googlevideo_origin_is_allowed(url) && url.path() == "/videoplayback"
}

fn youtube_googlevideo_origin_is_allowed(url: &Url) -> bool {
    url.scheme() == "https"
        && url
            .host_str()
            .is_some_and(|host| host == "googlevideo.com" || host.ends_with(".googlevideo.com"))
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none_or(|port| port == 443)
}

fn pick_thumbnail(thumbnails: &[rusty_ytdl::Thumbnail]) -> Option<Arc<str>> {
    thumbnails
        .iter()
        .max_by_key(|thumbnail| thumbnail.width * thumbnail.height)
        .map(|thumbnail| Arc::<str>::from(thumbnail.url.clone()))
}

fn parse_duration(length_seconds: &str, is_live: bool) -> Option<Duration> {
    if is_live {
        return None;
    }

    length_seconds.parse::<u64>().ok().map(Duration::from_secs)
}

fn looks_like_hls(stream_url: &str) -> bool {
    stream_url.contains(".m3u8")
}

fn format_url_expiry(stream_url: &str) -> Option<u64> {
    Url::parse(stream_url)
        .ok()?
        .query_pairs()
        .find(|(key, _)| key == "expire")
        .and_then(|(_, value)| value.parse::<u64>().ok())
}

fn earliest_expiry(first: Option<u64>, second: Option<u64>) -> Option<u64> {
    match (first, second) {
        (Some(first), Some(second)) => Some(first.min(second)),
        (Some(expiry), None) | (None, Some(expiry)) => Some(expiry),
        (None, None) => None,
    }
}

fn parse_content_length_from_url(stream_url: &str) -> Option<u64> {
    Url::parse(stream_url)
        .ok()?
        .query_pairs()
        .find(|(key, _)| key == "clen")
        .and_then(|(_, value)| value.parse::<u64>().ok())
}

fn youtube_video_id(request: &TrackRequest) -> Option<String> {
    if let Some(video_id) = request.canonical_key.strip_prefix("youtube:video:") {
        return Some(video_id.to_owned());
    }

    Url::parse(request.canonical_url.as_ref())
        .ok()?
        .query_pairs()
        .find(|(key, _)| key == "v")
        .map(|(_, value)| value.to_string())
}

#[derive(Clone, Debug, Deserialize)]
struct AndroidPlayerResponse {
    #[serde(rename = "streamingData")]
    streaming_data: Option<AndroidStreamingData>,
    #[serde(rename = "videoDetails")]
    video_details: Option<AndroidVideoDetails>,
    #[serde(rename = "playabilityStatus")]
    playability_status: Option<AndroidPlayabilityStatus>,
}

impl AndroidPlayerResponse {
    fn has_playable_stream(&self) -> bool {
        self.streaming_data.as_ref().is_some_and(|streaming_data| {
            choose_android_audio_stream(&streaming_data.adaptive_formats).is_some()
                || streaming_data
                    .hls_manifest_url
                    .as_ref()
                    .is_some_and(|url| !url.is_empty())
        })
    }

    fn has_cipher_stream(&self) -> bool {
        self.streaming_data.as_ref().is_some_and(|streaming_data| {
            streaming_data.adaptive_formats.iter().any(|format| {
                format.mime_type.starts_with("audio/")
                    && format
                        .signature_cipher
                        .as_ref()
                        .is_some_and(|cipher| !cipher.is_empty())
            })
        })
    }

    fn requires_fresh_visitor_session(&self) -> bool {
        self.playability_status
            .as_ref()
            .is_some_and(|status| status.status == "LOGIN_REQUIRED")
    }
}

#[derive(Clone, Debug, Deserialize)]
struct AndroidPlayabilityStatus {
    #[serde(default)]
    status: String,
}

#[derive(Clone, Debug, Deserialize)]
struct AndroidStreamingData {
    #[serde(rename = "adaptiveFormats", default)]
    adaptive_formats: Vec<AndroidAdaptiveFormat>,
    #[serde(rename = "hlsManifestUrl")]
    hls_manifest_url: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct AndroidAdaptiveFormat {
    #[serde(rename = "mimeType")]
    mime_type: String,
    url: Option<String>,
    #[serde(rename = "signatureCipher")]
    signature_cipher: Option<String>,
    bitrate: Option<u64>,
    #[serde(rename = "audioBitrate")]
    audio_bitrate: Option<u64>,
    #[serde(rename = "contentLength")]
    content_length: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct AndroidVideoDetails {
    #[serde(rename = "videoId", default)]
    video_id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    author: String,
    #[serde(rename = "lengthSeconds")]
    length_seconds: Option<String>,
    #[serde(rename = "isLiveContent", default)]
    is_live_content: bool,
    thumbnail: Option<AndroidThumbnailSet>,
}

#[derive(Clone, Debug, Deserialize)]
struct AndroidThumbnailSet {
    #[serde(default)]
    thumbnails: Vec<AndroidThumbnail>,
}

#[derive(Clone, Debug, Deserialize)]
struct AndroidThumbnail {
    url: String,
    width: Option<u64>,
    height: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wotoha_core::{PreparedSource, TrackMetadata, TrackRequest};

    const WORKER_PLAYER_FIXTURE: &str = r#"
var _player = {};
(function(g) {
  function Param() { this.values = new Map(); }
  Param.prototype.set = function(key, value) { this.values.set(key, value); };
  Param.prototype.get = function(key) { return this.values.get(key); };
  Param.prototype.clone = function() { return this; };
  Param.prototype.transform = function() {
    const n = this.values.get("n");
    if (n) this.values.set("n", n.slice(1) + n[0]);
  };
  function solve(a, b, c) {
    const value = new Param();
    value.set("alr", "yes");
    return value;
  }
})(_player);
"#;

    fn sample_format(
        url: &str,
        has_audio: bool,
        has_video: bool,
        audio_bitrate: Option<u64>,
    ) -> VideoFormat {
        serde_json::from_value(json!({
            "itag": 140,
            "mimeType": "audio/mp4; codecs=\"mp4a.40.2\"",
            "bitrate": 128000,
            "audioBitrate": audio_bitrate,
            "url": url,
            "hasVideo": has_video,
            "hasAudio": has_audio,
            "isLive": false,
            "isHLS": false,
            "isDashMPD": false
        }))
        .expect("sample youtube format should deserialize")
    }

    #[test]
    fn parses_youtube_expiry_from_query() {
        let url =
            "https://rr1---sn-a5mekn7k.googlevideo.com/videoplayback?expire=1777111066&id=o-AH";
        assert_eq!(format_url_expiry(url), Some(1777111066));
    }

    #[test]
    fn parses_current_visitor_session_fields() {
        let html = r#"{"sts": 20660,"visitorData":"abc\u003d\u003d","jsUrl":"\/s\/player\/b81a9a58\/player_ias.vflset\/en_US\/base.js"}"#;
        assert_eq!(extract_signature_timestamp(html), Some(20_660));
        assert_eq!(extract_visitor_data(html).as_deref(), Some("abc=="));
        assert_eq!(
            extract_player_url(html).as_deref(),
            Some("https://www.youtube.com/s/player/b81a9a58/player_ias.vflset/en_US/base.js")
        );
    }

    #[test]
    fn rejects_untrusted_player_javascript_url() {
        let html = r#"{"jsUrl":"https://example.com/s/player/b81a9a58/player_ias/base.js"}"#;
        assert!(extract_player_url(html).is_none());
    }

    #[tokio::test]
    #[ignore = "requires WOTOHA_YOUTUBE_JS_WORKER pointing to a built helper binary"]
    async fn isolated_javascript_worker_round_trips_and_restarts() {
        assert!(env::var_os("WOTOHA_YOUTUBE_JS_WORKER").is_some());
        let source = Arc::<str>::from(WORKER_PLAYER_FIXTURE);
        let solve = |value: &str| ChallengeInput {
            signature: None,
            n: Some(value.to_owned()),
        };
        let first = run_youtube_js_solver(
            "https://www.youtube.com/s/player/fixture/base.js",
            source.clone(),
            vec![solve("1234")],
        )
        .await
        .unwrap();
        assert_eq!(first[0].n.as_deref(), Some("2341"));

        let worker =
            YOUTUBE_JS_WORKER.get_or_init(|| AsyncMutex::new(JsWorkerSupervisor::default()));
        worker.lock().await.stop().await;
        let second = run_youtube_js_solver(
            "https://www.youtube.com/s/player/fixture/base.js",
            source,
            vec![solve("abcd")],
        )
        .await
        .unwrap();
        assert_eq!(second[0].n.as_deref(), Some("bcda"));
    }

    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "requires WOTOHA_YOUTUBE_JS_WORKER pointing to a built helper binary"]
    async fn javascript_worker_hot_swaps_after_atomic_replacement() {
        let original = env::var_os("WOTOHA_YOUTUBE_JS_WORKER")
            .map(PathBuf::from)
            .expect("set WOTOHA_YOUTUBE_JS_WORKER");
        let temporary = env::temp_dir().join(format!(
            "wotoha-worker-hot-swap-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&temporary).unwrap();
        let installed = temporary.join("wotoha-youtube-js-worker");
        fs::copy(&original, &installed).unwrap();
        unsafe {
            env::set_var("WOTOHA_YOUTUBE_JS_WORKER", &installed);
        }

        let source = Arc::<str>::from(WORKER_PLAYER_FIXTURE);
        let first = run_youtube_js_solver(
            "https://www.youtube.com/s/player/hot-swap/base.js",
            source.clone(),
            vec![ChallengeInput {
                signature: None,
                n: Some("1234".to_owned()),
            }],
        )
        .await
        .unwrap();
        assert_eq!(first[0].n.as_deref(), Some("2341"));
        let worker =
            YOUTUBE_JS_WORKER.get_or_init(|| AsyncMutex::new(JsWorkerSupervisor::default()));
        let first_lane = worker.lock().await.current.as_ref().cloned().unwrap();
        let first_pid = first_lane
            .process
            .lock()
            .await
            .as_ref()
            .and_then(|process| process.child.id())
            .unwrap();

        let candidate = temporary.join("wotoha-youtube-js-worker.new");
        fs::copy(&original, &candidate).unwrap();
        fs::rename(&candidate, &installed).unwrap();
        let second = run_youtube_js_solver(
            "https://www.youtube.com/s/player/hot-swap/base.js",
            source,
            vec![ChallengeInput {
                signature: None,
                n: Some("abcd".to_owned()),
            }],
        )
        .await
        .unwrap();
        assert_eq!(second[0].n.as_deref(), Some("bcda"));
        let mut supervisor = worker.lock().await;
        let second_lane = supervisor.current.as_ref().cloned().unwrap();
        let second_pid = second_lane
            .process
            .lock()
            .await
            .as_ref()
            .and_then(|process| process.child.id())
            .unwrap();
        assert_ne!(first_pid, second_pid);
        supervisor.stop().await;
        drop(supervisor);

        unsafe {
            env::set_var("WOTOHA_YOUTUBE_JS_WORKER", original);
        }
        fs::remove_dir_all(temporary).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "requires WOTOHA_YOUTUBE_JS_WORKER pointing to a built helper binary"]
    async fn javascript_worker_promotes_content_addressed_candidate() {
        use std::io::Write;

        let original = env::var_os("WOTOHA_YOUTUBE_JS_WORKER")
            .map(PathBuf::from)
            .expect("set WOTOHA_YOUTUBE_JS_WORKER");
        let root = env::temp_dir().join(format!(
            "wotoha-worker-candidate-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let original_bytes = fs::read(&original).unwrap();
        let current_id = Sha256::digest(&original_bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let mut candidate_bytes = original_bytes.clone();
        candidate_bytes.extend_from_slice(b"\nWOTOHA_CANDIDATE\n");
        let candidate_id = Sha256::digest(&candidate_bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        for (release_id, bytes) in [
            (&current_id, original_bytes.as_slice()),
            (&candidate_id, candidate_bytes.as_slice()),
        ] {
            let version = root.join("versions").join(release_id);
            fs::create_dir_all(&version).unwrap();
            let worker = version.join("wotoha-youtube-js-worker");
            let mut file = fs::File::create(&worker).unwrap();
            file.write_all(bytes).unwrap();
            fs::set_permissions(&worker, fs::metadata(&original).unwrap().permissions()).unwrap();
        }
        fs::write(root.join("current"), format!("{current_id}\n")).unwrap();
        fs::write(root.join("candidate"), format!("{candidate_id}\n")).unwrap();
        let ack = root.join("ack");
        unsafe {
            env::set_var("WOTOHA_YOUTUBE_JS_WORKER_DIR", &root);
            env::set_var("WOTOHA_YOUTUBE_JS_WORKER_ACK", &ack);
        }

        let output = run_youtube_js_solver(
            "https://www.youtube.com/s/player/candidate/base.js",
            Arc::<str>::from(WORKER_PLAYER_FIXTURE),
            vec![ChallengeInput {
                signature: None,
                n: Some("1234".to_owned()),
            }],
        )
        .await
        .unwrap();
        assert_eq!(output[0].n.as_deref(), Some("2341"));
        assert!(!ack.exists());
        let worker =
            YOUTUBE_JS_WORKER.get_or_init(|| AsyncMutex::new(JsWorkerSupervisor::default()));
        let mut supervisor = worker.lock().await;
        assert!(
            supervisor
                .current
                .as_ref()
                .unwrap()
                .executable
                .identity
                .contains(&current_id)
        );
        assert!(
            supervisor
                .candidate
                .as_ref()
                .unwrap()
                .executable
                .identity
                .contains(&candidate_id)
        );
        fs::write(root.join("current"), format!("{candidate_id}\n")).unwrap();
        fs::remove_file(root.join("candidate")).unwrap();
        drop(supervisor);
        let promoted = run_youtube_js_solver(
            "https://www.youtube.com/s/player/candidate/base.js",
            Arc::<str>::from(WORKER_PLAYER_FIXTURE),
            vec![ChallengeInput {
                signature: None,
                n: Some("abcd".to_owned()),
            }],
        )
        .await
        .unwrap();
        assert_eq!(promoted[0].n.as_deref(), Some("bcda"));
        let mut supervisor = worker.lock().await;
        assert!(
            supervisor
                .current
                .as_ref()
                .unwrap()
                .executable
                .identity
                .contains(&candidate_id)
        );
        supervisor.stop().await;
        drop(supervisor);

        unsafe {
            env::remove_var("WOTOHA_YOUTUBE_JS_WORKER_DIR");
            env::remove_var("WOTOHA_YOUTUBE_JS_WORKER_ACK");
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    #[ignore = "requires WOTOHA_YOUTUBE_PLAYER_JS_URL and live YouTube access"]
    async fn restricted_player_client_downloads_current_script() {
        let player_url =
            env::var("WOTOHA_YOUTUBE_PLAYER_JS_URL").expect("set Player JavaScript URL");
        let source = youtube_player_source(&player_url).await.unwrap();
        assert!(source.len() > 1024);
        assert!(source.len() <= YOUTUBE_PLAYER_SCRIPT_MAX_BYTES);
    }

    #[test]
    fn supports_common_youtube_hosts() {
        let provider = YouTubeProvider;
        assert!(provider.supports("https://www.youtube.com/watch?v=dQw4w9WgXcQ"));
        assert!(provider.supports("https://youtu.be/dQw4w9WgXcQ"));
        assert!(!provider.supports("https://example.com/watch?v=dQw4w9WgXcQ"));
    }

    #[test]
    fn extracts_youtube_video_id_from_request_key() {
        let request = TrackRequest::new(
            "youtube",
            "youtube:video:dQw4w9WgXcQ",
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            PreparedSource::hls(
                "https://manifest.googlevideo.com/api/manifest/hls_playlist",
                Vec::new(),
                None,
            ),
            TrackMetadata::new(
                "Never Gonna Give You Up",
                "Rick Astley",
                "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
                None,
                None,
            ),
        );

        assert_eq!(youtube_video_id(&request), Some("dQw4w9WgXcQ".to_owned()));
    }

    #[test]
    fn extracts_youtube_video_id_from_supported_urls() {
        assert_eq!(
            youtube_video_id_from_url("https://www.youtube.com/watch?v=dQw4w9WgXcQ"),
            Some("dQw4w9WgXcQ".to_owned())
        );
        assert_eq!(
            youtube_video_id_from_url("https://youtu.be/dQw4w9WgXcQ?t=10"),
            Some("dQw4w9WgXcQ".to_owned())
        );
        assert_eq!(
            youtube_video_id_from_url("https://www.youtube.com/shorts/dQw4w9WgXcQ"),
            Some("dQw4w9WgXcQ".to_owned())
        );
        assert_eq!(
            youtube_video_id_from_url("https://example.com/watch?v=dQw4w9WgXcQ"),
            None
        );
    }

    #[test]
    fn prefers_non_empty_playable_youtube_urls() {
        let options = VideoOptions {
            quality: VideoQuality::HighestAudio,
            filter: VideoSearchOptions::Audio,
            ..Default::default()
        };
        let formats = vec![
            sample_format("", true, false, Some(160)),
            sample_format("https://example.com/audio.m4a", true, false, Some(128)),
        ];

        let chosen = choose_playable_format(&formats, &None, &options).unwrap();
        assert_eq!(chosen.stream_url, "https://example.com/audio.m4a");
        assert!(!chosen.is_hls);
    }

    #[test]
    fn falls_back_to_manifest_when_all_youtube_urls_are_empty() {
        let options = VideoOptions {
            quality: VideoQuality::HighestAudio,
            filter: VideoSearchOptions::Audio,
            ..Default::default()
        };
        let formats = vec![sample_format("", true, false, Some(160))];

        let chosen = choose_playable_format(
            &formats,
            &Some("https://example.com/master.m3u8".to_owned()),
            &options,
        )
        .unwrap();
        assert_eq!(chosen.stream_url, "https://example.com/master.m3u8");
        assert!(chosen.is_hls);
    }

    #[test]
    fn falls_back_to_muxed_youtube_format_when_audio_only_urls_are_missing() {
        let options = VideoOptions {
            quality: VideoQuality::HighestAudio,
            filter: VideoSearchOptions::Audio,
            ..Default::default()
        };
        let formats = vec![
            sample_format("", true, false, Some(160)),
            sample_format("https://example.com/muxed.mp4", true, true, None),
        ];

        let chosen = choose_playable_format(&formats, &None, &options).unwrap();
        assert_eq!(chosen.stream_url, "https://example.com/muxed.mp4");
    }

    #[test]
    fn parses_youtube_content_length_from_query() {
        let url = "https://example.com/videoplayback?clen=2891031&expire=1777072413";
        assert_eq!(parse_content_length_from_url(url), Some(2_891_031));
    }

    #[test]
    fn prefers_opus_android_audio_when_available() {
        let formats = vec![
            AndroidAdaptiveFormat {
                mime_type: "audio/mp4; codecs=\"mp4a.40.2\"".to_owned(),
                url: Some("https://example.com/aac".to_owned()),
                signature_cipher: None,
                bitrate: Some(128_000),
                audio_bitrate: Some(128),
                content_length: None,
            },
            AndroidAdaptiveFormat {
                mime_type: "audio/webm; codecs=\"opus\"".to_owned(),
                url: Some("https://example.com/opus".to_owned()),
                signature_cipher: None,
                bitrate: Some(160_000),
                audio_bitrate: Some(160),
                content_length: None,
            },
        ];

        let chosen = choose_android_audio_stream(&formats).expect("android audio format");
        assert_eq!(chosen.url.as_deref(), Some("https://example.com/opus"));
    }

    #[test]
    fn shipped_native_client_manifest_is_valid() {
        let profiles: Vec<NativeClientProfile> =
            serde_json::from_str(include_str!("../../../../deploy/youtube-clients.json"))
                .expect("shipped YouTube client manifest should parse");

        assert!(validate_native_client_profiles(&profiles));
        assert!(profiles.iter().any(|profile| profile.id == "android_vr"));
    }

    #[test]
    fn rejects_unsafe_native_client_ids() {
        let mut profiles = default_native_client_profiles();
        profiles[0].id = "../visionos".to_owned();
        assert!(!validate_native_client_profiles(&profiles));
    }

    #[test]
    fn rejects_duplicate_native_client_ids() {
        let mut profiles = default_native_client_profiles();
        profiles[1].id = profiles[0].id.clone();
        assert!(!validate_native_client_profiles(&profiles));
    }

    #[test]
    fn login_required_invalidates_cached_visitor_session() {
        let response: AndroidPlayerResponse = serde_json::from_value(json!({
            "playabilityStatus": { "status": "LOGIN_REQUIRED" }
        }))
        .unwrap();
        assert!(response.requires_fresh_visitor_session());
    }

    #[test]
    fn player_po_token_uses_service_integrity_dimensions() {
        let profile = default_native_client_profiles().remove(0);
        let request = native_player_request("video-id", &profile, None, None, Some("player-token"));
        assert_eq!(
            request.pointer("/serviceIntegrityDimensions/poToken"),
            Some(&json!("player-token"))
        );

        let request = native_player_request("video-id", &profile, None, None, None);
        assert!(request.pointer("/serviceIntegrityDimensions").is_none());
    }

    #[test]
    fn gvs_query_token_replaces_only_existing_pot() {
        let updated = url_with_po_token(
            "https://example.com/videoplayback?sig=a%2Fb&foo=x+z&pot=old#part",
            "new/token",
        )
        .unwrap();
        assert_eq!(
            updated,
            "https://example.com/videoplayback?sig=a%2Fb&foo=x+z&pot=new%2Ftoken#part"
        );
    }

    #[test]
    fn query_value_replacement_preserves_signed_query_bytes() {
        let updated = url_with_query_value(
            "https://example.com/videoplayback?sig=a%2Fb&n=old+value&foo=x%20z#part",
            "n",
            "new/value",
        )
        .unwrap();
        assert_eq!(
            updated,
            "https://example.com/videoplayback?sig=a%2Fb&foo=x%20z&n=new%2Fvalue#part"
        );
    }

    #[test]
    fn hls_path_n_replacement_preserves_signed_query_bytes() {
        let raw = "https://manifest.googlevideo.com/api/manifest/hls_playlist/n/old-value/sig/a%2Fb?foo=x+z#part";
        let parsed = Url::parse(raw).unwrap();
        assert_eq!(path_n_challenge(&parsed).as_deref(), Some("old-value"));
        assert_eq!(
            url_with_path_n_value(raw, "new/value").unwrap(),
            "https://manifest.googlevideo.com/api/manifest/hls_playlist/n/new%2Fvalue/sig/a%2Fb?foo=x+z#part"
        );
    }

    #[test]
    fn player_key_changes_with_source_content() {
        let url = "https://www.youtube.com/s/player/test/base.js";
        assert_ne!(
            youtube_player_key(url, "first"),
            youtube_player_key(url, "second")
        );
    }

    #[test]
    fn content_addressed_worker_requires_matching_digest() {
        let root = env::temp_dir().join(format!(
            "wotoha-worker-digest-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let bytes = b"fixture worker";
        let release_id = Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let worker_name = if cfg!(windows) {
            "wotoha-youtube-js-worker.exe"
        } else {
            "wotoha-youtube-js-worker"
        };
        let version = root.join("versions").join(&release_id);
        fs::create_dir_all(&version).unwrap();
        fs::write(version.join(worker_name), bytes).unwrap();
        fs::write(root.join("current"), format!("{release_id}\n")).unwrap();

        assert_eq!(
            read_js_worker_pointer(&root.join("current"))
                .unwrap()
                .as_deref(),
            Some(release_id.as_str())
        );
        assert!(content_addressed_js_worker(&root, &release_id).is_ok());
        fs::write(version.join(worker_name), b"tampered worker").unwrap();
        assert!(content_addressed_js_worker(&root, &release_id).is_err());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn invalid_candidate_pointer_does_not_block_content_addressed_current() {
        let root = env::temp_dir().join(format!(
            "wotoha-worker-invalid-candidate-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let bytes = b"fixture current worker";
        let release_id = Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let worker_name = if cfg!(windows) {
            "wotoha-youtube-js-worker.exe"
        } else {
            "wotoha-youtube-js-worker"
        };
        let version = root.join("versions").join(&release_id);
        fs::create_dir_all(&version).unwrap();
        fs::write(version.join(worker_name), bytes).unwrap();
        fs::write(root.join("current"), format!("{release_id}\n")).unwrap();
        fs::write(root.join("candidate"), "invalid-candidate\n").unwrap();

        let selection = content_addressed_js_worker_selection(root.clone()).unwrap();
        assert_eq!(
            selection.current.path,
            fs::canonicalize(version.join(worker_name)).unwrap()
        );
        assert!(selection.candidate.is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn worker_ack_is_promoted_atomically() {
        let root = env::temp_dir().join(format!(
            "wotoha-worker-ack-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let ack = root.join("ack");
        let release_id = "a".repeat(64);
        write_js_worker_ack(&ack, &release_id).unwrap();
        assert_eq!(fs::read_to_string(&ack).unwrap(), format!("{release_id}\n"));
        assert_eq!(
            fs::read_dir(&root).unwrap().filter_map(Result::ok).count(),
            1
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn validated_worker_ack_retries_after_write_failure_and_rejects_stale_proof() {
        let root = env::temp_dir().join(format!(
            "wotoha-worker-ack-retry-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let current = JsWorkerExecutable {
            path: PathBuf::from("current"),
            app_worker_mode: false,
            identity: "current-identity".to_owned(),
        };
        let candidate_executable = JsWorkerExecutable {
            path: PathBuf::from("candidate"),
            app_worker_mode: false,
            identity: "candidate-identity".to_owned(),
        };
        let release_id = "a".repeat(64);
        let proof = JsWorkerCandidateProof {
            release_id: release_id.clone(),
            ack_path: root.join("ack"),
            executable_identity: candidate_executable.identity.clone(),
            baseline_current_identity: current.identity.clone(),
            player_key: "player".to_owned(),
        };
        let pending = JsWorkerSelection {
            current: current.clone(),
            candidate: Some(JsWorkerCandidate {
                executable: candidate_executable.clone(),
                release_id: release_id.clone(),
                ack_path: proof.ack_path.clone(),
            }),
        };
        assert!(write_validated_js_worker_candidate_ack(&pending, &proof).is_err());
        fs::create_dir(&root).unwrap();
        assert_eq!(
            write_validated_js_worker_candidate_ack(&pending, &proof),
            Ok(true)
        );
        assert_eq!(
            fs::read_to_string(&proof.ack_path).unwrap(),
            format!("{release_id}\n")
        );

        let promoted = JsWorkerSelection {
            current: candidate_executable,
            candidate: None,
        };
        assert_eq!(
            write_validated_js_worker_candidate_ack(&promoted, &proof),
            Ok(false)
        );
        let stale_current = JsWorkerSelection {
            current: JsWorkerExecutable {
                path: PathBuf::from("new-current"),
                app_worker_mode: false,
                identity: "new-current-identity".to_owned(),
            },
            candidate: pending.candidate,
        };
        assert_eq!(
            write_validated_js_worker_candidate_ack(&stale_current, &proof),
            Ok(false)
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn worker_candidate_must_not_regress_a_current_success() {
        let output = |value: &str| {
            Ok(ChallengeOutput {
                signature: None,
                n: Some(value.to_owned()),
            })
        };
        assert!(js_worker_candidate_preserves_current_successes(
            &[output("same"), output("new")],
            &[output("same"), Err("old failure".to_owned())]
        ));
        assert!(js_worker_candidate_preserves_current_successes(
            &[output("different")],
            &[output("same")]
        ));
        assert!(!js_worker_candidate_preserves_current_successes(
            &[Err("new failure".to_owned())],
            &[output("same")]
        ));
        assert!(!js_worker_candidate_preserves_current_successes(
            &[Err("new failure".to_owned())],
            &[Err("old failure".to_owned())]
        ));
    }

    #[test]
    fn worker_candidate_rejection_is_player_scoped_and_expires() {
        let now = Instant::now();
        let rejected_key = WorkerPlayerKey {
            executable_identity: "candidate-a".to_owned(),
            player_key: "player-a".to_owned(),
        };
        let mut rejected = HashMap::new();
        rejected.insert(
            rejected_key.clone(),
            now + YOUTUBE_JS_WORKER_CANDIDATE_REJECTION_TTL,
        );
        assert!(js_worker_candidate_is_rejected(
            &rejected,
            &rejected_key,
            now
        ));
        assert!(!js_worker_candidate_is_rejected(
            &rejected,
            &WorkerPlayerKey {
                executable_identity: "candidate-a".to_owned(),
                player_key: "player-b".to_owned(),
            },
            now
        ));
        assert!(!js_worker_candidate_is_rejected(
            &rejected,
            &WorkerPlayerKey {
                executable_identity: "candidate-b".to_owned(),
                player_key: "player-a".to_owned(),
            },
            now
        ));
        assert!(!js_worker_candidate_is_rejected(
            &rejected,
            &rejected_key,
            now + YOUTUBE_JS_WORKER_CANDIDATE_REJECTION_TTL
        ));
    }

    #[test]
    fn official_current_reconcile_ignores_candidate_rejection_and_old_backoff() {
        let now = Instant::now();
        let mut supervisor = JsWorkerSupervisor::default();
        supervisor.current = Some(Arc::new(JsWorkerLane::new(JsWorkerExecutable {
            path: PathBuf::from("old-current"),
            app_worker_mode: false,
            identity: "old-current".to_owned(),
        })));
        supervisor.failure_until.insert(
            WorkerPlayerKey {
                executable_identity: "old-current".to_owned(),
                player_key: "player".to_owned(),
            },
            now + YOUTUBE_JS_WORKER_FAILURE_BACKOFF,
        );
        supervisor.rejected_candidates.insert(
            WorkerPlayerKey {
                executable_identity: "promoted-candidate".to_owned(),
                player_key: "player".to_owned(),
            },
            now + YOUTUBE_JS_WORKER_CANDIDATE_REJECTION_TTL,
        );
        let lane = supervisor.reconcile_current(JsWorkerExecutable {
            path: PathBuf::from("promoted-current"),
            app_worker_mode: false,
            identity: "promoted-candidate".to_owned(),
        });
        assert_eq!(lane.executable.identity, "promoted-candidate");
        assert!(supervisor.failure_until.is_empty());
    }

    #[test]
    fn worker_response_request_id_rejects_stale_echo_and_accepts_legacy() {
        assert!(validate_js_worker_response_request_id(Some(7), 7).is_ok());
        assert!(validate_js_worker_response_request_id(None, 7).is_ok());
        assert!(validate_js_worker_response_request_id(Some(6), 7).is_err());
    }

    #[tokio::test]
    async fn uncommitted_worker_process_lease_leaves_the_lane_empty() {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .arg("--help")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let process = JsWorkerProcess {
            stdin: child.stdin.take().unwrap(),
            stdout: child.stdout.take().unwrap(),
            child,
            loaded_players: HashSet::new(),
            next_request_id: 1,
        };
        let slot = Arc::new(AsyncMutex::new(None));
        let guard = slot.clone().lock_owned().await;
        drop(JsWorkerProcessLease::new(guard, process));
        assert!(slot.lock().await.is_none());
    }

    #[test]
    fn hls_token_uses_manifest_path() {
        let updated = hls_url_with_po_token(
            "https://example.com/manifest/playlist.m3u8?sig=a%2Fb#part",
            "new/token",
        )
        .unwrap();
        assert_eq!(
            updated,
            "https://example.com/manifest/playlist.m3u8/pot/new%2Ftoken?sig=a%2Fb#part"
        );
        let replaced = hls_url_with_po_token(&updated, "fresh").unwrap();
        assert_eq!(
            replaced,
            "https://example.com/manifest/playlist.m3u8/pot/fresh?sig=a%2Fb#part"
        );
    }

    #[test]
    fn po_token_shortens_prepared_source_expiry() {
        let mut prepared = PreparedSource::http(
            "https://example.com/videoplayback?expire=2000",
            Vec::new(),
            Some(10_000),
            Some(2_000),
        );
        let token = PoToken {
            value: "token".to_owned(),
            expires_at: Instant::now() + Duration::from_secs(60),
            expires_at_unix: 1_500,
        };
        add_po_token_to_prepared_source(&mut prepared, &token).unwrap();
        assert_eq!(prepared.expires_at_unix(), Some(1_500));
    }

    #[test]
    fn cipher_only_formats_do_not_break_player_response_parsing() {
        let response: AndroidPlayerResponse = serde_json::from_value(json!({
            "streamingData": {
                "adaptiveFormats": [{
                    "mimeType": "audio/webm; codecs=\"opus\"",
                    "signatureCipher": "s=encrypted&sp=sig&url=https%3A%2F%2Fr1.googlevideo.com%2Fvideoplayback%3Fn%3Dold"
                }]
            }
        }))
        .unwrap();
        assert!(!response.has_playable_stream());
        assert!(response.has_cipher_stream());
        let cipher = response.streaming_data.as_ref().unwrap().adaptive_formats[0]
            .signature_cipher
            .as_deref()
            .unwrap();
        let job = cipher_format_job(0, cipher).unwrap().unwrap();
        assert_eq!(job.signature_parameter, "sig");
        assert_eq!(job.input.signature.as_deref(), Some("encrypted"));
        assert_eq!(job.input.n.as_deref(), Some("old"));
    }

    #[test]
    fn cipher_output_failure_is_isolated_to_its_format() {
        let jobs = (0..3)
            .map(|format_index| CipherFormatJob {
                format_index,
                stream_url: format!("https://r{format_index}.googlevideo.com/videoplayback?n=old"),
                signature_parameter: "sig".to_owned(),
                input: ChallengeInput {
                    signature: (format_index < 2).then(|| "encrypted".to_owned()),
                    n: Some("old".to_owned()),
                },
            })
            .collect::<Vec<_>>();
        let targets = vec![
            CipherOutputTarget::Signature(0),
            CipherOutputTarget::N(0),
            CipherOutputTarget::Signature(1),
            CipherOutputTarget::N(1),
            CipherOutputTarget::N(2),
        ];
        let outputs = vec![
            Ok(ChallengeOutput {
                signature: Some("sig-0".to_owned()),
                n: None,
            }),
            Ok(ChallengeOutput {
                signature: None,
                n: Some("n-0".to_owned()),
            }),
            Ok(ChallengeOutput {
                signature: Some("sig-1".to_owned()),
                n: None,
            }),
            Err("no_unique_solution".to_owned()),
            Ok(ChallengeOutput {
                signature: None,
                n: Some("n-2".to_owned()),
            }),
        ];

        let solved = apply_cipher_outputs(&jobs, targets, outputs).unwrap();
        let first = Url::parse(solved[0].as_ref().unwrap()).unwrap();
        assert_eq!(
            first
                .query_pairs()
                .find(|(key, _)| key == "sig")
                .map(|(_, value)| value.into_owned())
                .as_deref(),
            Some("sig-0")
        );
        assert_eq!(
            first
                .query_pairs()
                .find(|(key, _)| key == "n")
                .map(|(_, value)| value.into_owned())
                .as_deref(),
            Some("n-0")
        );
        assert!(solved[1].is_err());
        assert!(solved[2].as_ref().unwrap().contains("n=n-2"));
    }
}
