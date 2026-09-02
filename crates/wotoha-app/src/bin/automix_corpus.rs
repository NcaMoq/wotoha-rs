use std::{
    collections::{HashMap, HashSet},
    env,
    error::Error,
    ffi::OsString,
    fs,
    future::Future,
    path::{Path, PathBuf},
    process,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use ebur128::{EbuR128, Mode};
use reqwest::{
    Client, StatusCode, Url,
    header::{
        CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, RANGE,
    },
    redirect::Policy,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use songbird::Songbird;
use wotoha_core::{
    PreparedRangeMode, PreparedSource, TrackRequest,
    audio_analysis::LowBandFilter,
    automix::{
        AutoMixConfig, AutoMixQualityReport, GuardedTransitionPlan, TrackAnalysis, TransitionKind,
        explain_beatmatch_decision, plan_guarded_transition_with_base_gains,
    },
    config::LoudnessConfig,
    loudness::loudness_normalization_gain,
    url::is_allowed_runtime_redirect_url,
};
use wotoha_media::MediaResolver;
use wotoha_runtime::{AnalysisBackend, SongbirdRuntime};

const NETWORK_OPT_IN: &str = "WOTOHA_AUTOMIX_CORPUS_ALLOW_NETWORK";
const ANALYSIS_CACHE_ENV: &str = "WOTOHA_ANALYSIS_CACHE_DIR";
const RUN_MARKER: &str = ".wotoha-automix-corpus-run";
const MAX_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
const HASH_REQUEST_RETRIES: usize = 2;
const RMS_WINDOW_MS: u32 = 500;
const RMS_HOP_MS: u32 = 250;
const SILENCE_HOP_MS: u32 = 10;
const RESOLVE_STAGE_TIMEOUT: Duration = Duration::from_secs(90);
const PREPARE_STAGE_TIMEOUT: Duration = Duration::from_secs(90);
const CONTENT_HASH_STAGE_TIMEOUT: Duration = Duration::from_secs(180);
const ANALYZE_STAGE_TIMEOUT: Duration = Duration::from_secs(300);
const PAIR_RENDER_STAGE_TIMEOUT: Duration = Duration::from_secs(180);
const FIXTURE_TOTAL_TIMEOUT: Duration = Duration::from_secs(900);
const MAX_ACQUISITION_CYCLES: usize = 3;

type AnyError = Box<dyn Error + Send + Sync>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct StageTimeout {
    code: &'static str,
}

async fn run_stage_until<F, T>(
    future: F,
    deadline: Instant,
    code: &'static str,
) -> Result<T, StageTimeout>
where
    F: Future<Output = T>,
{
    tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), future)
        .await
        .map_err(|_| StageTimeout { code })
}

fn stage_start(scope: &str, id: &str, stage: &str) -> Instant {
    eprintln!("automix stage={stage} {scope}={id} begin");
    Instant::now()
}

fn stage_end(scope: &str, id: &str, stage: &str, started: Instant, result: &str) {
    eprintln!(
        "automix stage={stage} {scope}={id} elapsed_ms={} result={result}",
        started.elapsed().as_millis()
    );
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    match run().await {
        Ok(true) => {}
        Ok(false) => process::exit(1),
        Err(error) => {
            eprintln!("AutoMix corpus evaluator failed: {error}");
            process::exit(2);
        }
    }
}

async fn run() -> Result<bool, AnyError> {
    let original_dir = env::current_dir()?;
    let options = Options::parse(env::args().skip(1), &original_dir)?;
    if !options.allow_network || env::var(NETWORK_OPT_IN).as_deref() != Ok("1") {
        return Err(
            format!("network execution requires --allow-network and {NETWORK_OPT_IN}=1").into(),
        );
    }

    let manifest_bytes = fs::read(&options.manifest)?;
    let manifest: CorpusManifest = serde_json::from_slice(&manifest_bytes)?;
    validate_manifest(&manifest)?;
    let manifest_sha256 = sha256_hex(&manifest_bytes);
    let run_dir = create_run_dir()?;
    let _run_environment =
        match RunEnvironment::new(&original_dir, &run_dir, options.keep_artifacts) {
            Ok(environment) => environment,
            Err(error) => {
                if let Err(cleanup_error) = remove_run_dir(&run_dir) {
                    eprintln!("warning: could not clean failed run setup: {cleanup_error}");
                }
                return Err(error);
            }
        };
    let preview_dir = run_dir.join("previews");
    fs::create_dir(&preview_dir)?;

    let resolver = MediaResolver::new()?;
    let runtime = SongbirdRuntime::new(Songbird::serenity())?;
    let http = corpus_http_client()?;
    let automix = automix_config();
    let loudness = loudness_config();
    let mut prepared = HashMap::new();
    let mut track_reports = Vec::with_capacity(manifest.tracks.len());

    for fixture in &manifest.tracks {
        let (track, report) = prepare_track(fixture, &resolver, &runtime, &http).await;
        if let Some(track) = track {
            prepared.insert(fixture.id.clone(), track);
        }
        track_reports.push(report);
    }

    let mut pair_reports = Vec::with_capacity(manifest.pairs.len());
    for fixture in &manifest.pairs {
        pair_reports.push(
            evaluate_pair(
                fixture,
                &prepared,
                &runtime,
                &automix,
                &loudness,
                &manifest.thresholds,
                &preview_dir,
            )
            .await,
        );
    }

    let passed = pair_reports.iter().all(|pair| pair.passed)
        && track_reports.iter().all(|track| track.ready);
    let report = CorpusReport {
        schema_version: 1,
        passed,
        manifest_sha256,
        run_directory: run_dir.display().to_string(),
        config: ConfigReport::from_configs(&automix, &loudness),
        thresholds: manifest.thresholds,
        tracks: track_reports,
        pairs: pair_reports,
    };
    let json = serde_json::to_string_pretty(&report)?;
    let temporary_report = run_dir.join("report.json");
    fs::write(&temporary_report, json.as_bytes())?;
    if let Some(path) = options.report {
        fs::write(path, json.as_bytes())?;
    }
    println!("{json}");
    Ok(passed)
}

#[derive(Debug)]
struct Options {
    allow_network: bool,
    manifest: PathBuf,
    report: Option<PathBuf>,
    keep_artifacts: bool,
}

impl Options {
    fn parse(args: impl IntoIterator<Item = String>, cwd: &Path) -> Result<Self, AnyError> {
        let mut allow_network = false;
        let mut manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("e2e")
            .join("automix-corpus.json");
        let mut report = None;
        let mut keep_artifacts = false;
        let mut args = args.into_iter();
        while let Some(argument) = args.next() {
            match argument.as_str() {
                "--allow-network" => allow_network = true,
                "--manifest" => {
                    manifest = args
                        .next()
                        .map(PathBuf::from)
                        .ok_or("--manifest requires a path")?;
                }
                "--report" => {
                    report = Some(
                        args.next()
                            .map(PathBuf::from)
                            .ok_or("--report requires a path")?,
                    );
                }
                "--keep-artifacts" => keep_artifacts = true,
                "--help" | "-h" => {
                    return Err(
                        "usage: automix_corpus --allow-network [--manifest PATH] [--report PATH] [--keep-artifacts]"
                            .into(),
                    );
                }
                _ => return Err(format!("unknown option: {argument}").into()),
            }
        }
        Ok(Self {
            allow_network,
            manifest: absolute_path(cwd, manifest),
            report: report.map(|path| absolute_path(cwd, path)),
            keep_artifacts,
        })
    }
}

fn absolute_path(cwd: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

fn create_run_dir() -> Result<PathBuf, AnyError> {
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
    for sequence in 0..32_u8 {
        let candidate = env::temp_dir().join(format!(
            "wotoha-automix-corpus-{timestamp}-{}-{sequence}",
            process::id()
        ));
        match fs::create_dir(&candidate) {
            Ok(()) => {
                if fs::write(candidate.join(RUN_MARKER), b"wotoha-automix-corpus\n").is_ok() {
                    return Ok(candidate);
                }
                let _ = fs::remove_dir(&candidate);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    Err("could not allocate a unique temporary run directory".into())
}

/// Owns the process-global environment changes made by this single-process CLI.  The runtime
/// reads the analysis-cache location while it is constructed, so this guard is installed before
/// `SongbirdRuntime::new` and restores both the cache variable and working directory on every
/// return path (including errors and panics during unwinding).
struct RunEnvironment {
    original_dir: PathBuf,
    original_cache: Option<OsString>,
    run_dir: PathBuf,
    keep_artifacts: bool,
}

impl RunEnvironment {
    fn new(original_dir: &Path, run_dir: &Path, keep_artifacts: bool) -> Result<Self, AnyError> {
        env::set_current_dir(run_dir)?;
        let original_cache = env::var_os(ANALYSIS_CACHE_ENV);
        // `set_var` is process-global and therefore unsafe in Rust 2024.  This binary is a
        // single-process, current-thread CLI; no application work runs until after this value is
        // set, and the guard restores it before the process leaves the runner.
        unsafe { env::set_var(ANALYSIS_CACHE_ENV, analysis_cache_dir(run_dir)) };
        Ok(Self {
            original_dir: original_dir.to_owned(),
            original_cache,
            run_dir: run_dir.to_owned(),
            keep_artifacts,
        })
    }
}

impl Drop for RunEnvironment {
    fn drop(&mut self) {
        let restore_result = env::set_current_dir(&self.original_dir);
        unsafe {
            match &self.original_cache {
                Some(value) => env::set_var(ANALYSIS_CACHE_ENV, value),
                None => env::remove_var(ANALYSIS_CACHE_ENV),
            }
        }
        if let Err(error) = restore_result {
            eprintln!("warning: could not restore working directory: {error}");
        }
        if self.keep_artifacts {
            return;
        }
        if let Err(error) = remove_run_dir(&self.run_dir) {
            eprintln!(
                "warning: could not clean AutoMix corpus artifacts at {}: {error}",
                self.run_dir.display()
            );
        }
    }
}

fn analysis_cache_dir(run_dir: &Path) -> PathBuf {
    run_dir.join(".wotoha-analysis")
}

fn remove_run_dir(run_dir: &Path) -> Result<(), AnyError> {
    let temp_dir = env::temp_dir();
    let is_exact_child = run_dir.parent() == Some(temp_dir.as_path())
        && run_dir
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("wotoha-automix-corpus-"));
    if !is_exact_child {
        return Err("refusing to clean a path that is not an exact AutoMix temp child".into());
    }
    let metadata = fs::symlink_metadata(run_dir)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err("refusing to clean a non-directory AutoMix temp child".into());
    }
    let marker = run_dir.join(RUN_MARKER);
    let marker_metadata = fs::symlink_metadata(&marker)?;
    if !marker_metadata.file_type().is_file() || marker_metadata.file_type().is_symlink() {
        return Err("refusing to clean an unowned AutoMix temp child".into());
    }
    fs::remove_dir_all(run_dir)?;
    Ok(())
}

#[derive(Clone, Debug, Deserialize)]
struct CorpusManifest {
    schema_version: u32,
    tracks: Vec<TrackFixture>,
    pairs: Vec<PairFixture>,
    thresholds: Thresholds,
}

#[derive(Clone, Debug, Deserialize)]
struct TrackFixture {
    id: String,
    title: String,
    genre: String,
    url: String,
    expected_video_id: String,
    expected_title_tokens: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct PairFixture {
    outgoing: String,
    incoming: String,
    expected_transition_kind: String,
}

fn normalized_identity_tokens(value: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut token = String::new();
    for character in value.chars().map(|character| {
        if ('\u{FF01}'..='\u{FF5E}').contains(&character) {
            char::from_u32(character as u32 - 0xFEE0).unwrap_or(character)
        } else if character == '\u{3000}' {
            ' '
        } else {
            character
        }
    }) {
        if character.is_alphanumeric() {
            token.extend(character.to_lowercase());
        } else if !token.is_empty() {
            tokens.push(std::mem::take(&mut token));
        }
    }
    if !token.is_empty() {
        tokens.push(token);
    }
    tokens
}

fn youtube_video_id(url: &str) -> Option<String> {
    let url = Url::parse(url).ok()?;
    let host = url.host_str()?.to_ascii_lowercase();
    if host == "youtu.be" {
        return url
            .path_segments()?
            .find(|segment| !segment.is_empty())
            .map(str::to_owned);
    }
    if host != "youtube.com" && !host.ends_with(".youtube.com") {
        return None;
    }
    if let Some((_, value)) = url.query_pairs().find(|(key, _)| key == "v")
        && !value.is_empty()
    {
        return Some(value.into_owned());
    }
    let mut segments = url.path_segments()?.filter(|segment| !segment.is_empty());
    match segments.next()? {
        "shorts" | "live" | "embed" => segments.next().map(str::to_owned),
        _ => None,
    }
}

fn validate_fixture_identity(
    fixture: &TrackFixture,
    request: &TrackRequest,
) -> Result<(), &'static str> {
    let resolved_tokens = normalized_identity_tokens(request.metadata.title.as_ref())
        .into_iter()
        .collect::<HashSet<_>>();
    if let Some(missing) = fixture
        .expected_title_tokens
        .iter()
        .flat_map(|token| normalized_identity_tokens(token))
        .find(|token| !resolved_tokens.contains(token))
    {
        return Err(if missing == "extended" || missing == "mix" {
            "identity_title_missing_extended_mix"
        } else {
            "identity_title_token_mismatch"
        });
    }
    let canonical_id = youtube_video_id(request.canonical_url.as_ref());
    let key_id = request
        .canonical_key
        .strip_prefix("youtube:video:")
        .map(str::to_owned);
    if canonical_id.as_deref() != Some(fixture.expected_video_id.as_str())
        || key_id.as_deref() != Some(fixture.expected_video_id.as_str())
    {
        return Err("identity_video_id_mismatch");
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
struct Thresholds {
    min_kick_coverage: f32,
    min_matched_kicks: usize,
    kick_phase_p95_ms: f32,
    kick_phase_max_ms: f32,
    kick_handoff_ms: f32,
    minimum_rms_gap_ratio: f32,
    silence_window_ms: u32,
    silence_threshold_dbfs: f32,
    maximum_sample_peak_dbfs: f32,
    maximum_true_peak_dbtp: f32,
}

fn validate_manifest(manifest: &CorpusManifest) -> Result<(), AnyError> {
    if manifest.schema_version != 1 {
        return Err("unsupported corpus manifest schema".into());
    }
    if manifest.tracks.len() < 4 {
        return Err("corpus must contain the reported track plus three comparisons".into());
    }
    let mut ids = HashSet::new();
    for track in &manifest.tracks {
        if track.id.is_empty()
            || track.title.is_empty()
            || track.genre.is_empty()
            || track.expected_video_id.is_empty()
            || track.expected_title_tokens.is_empty()
            || !ids.insert(track.id.as_str())
        {
            return Err(
                "track ids, titles, genres, expected video ids, and title tokens must be non-empty and unique"
                    .into(),
            );
        }
        if track
            .expected_title_tokens
            .iter()
            .any(|token| normalized_identity_tokens(token).is_empty())
        {
            return Err("expected title tokens must contain searchable text".into());
        }
        let url = Url::parse(&track.url)?;
        if url.scheme() != "https" || url.host_str().is_none() {
            return Err("corpus track URLs must be absolute HTTPS URLs".into());
        }
        if youtube_video_id(&track.url).as_deref() != Some(track.expected_video_id.as_str()) {
            return Err("corpus URL and expected YouTube video id differ".into());
        }
    }
    let expected_ids = manifest
        .tracks
        .iter()
        .map(|track| track.id.as_str())
        .collect::<HashSet<_>>();
    let mut self_pairs = HashSet::new();
    let mut cross_pairs = 0;
    for pair in &manifest.pairs {
        if !expected_ids.contains(pair.outgoing.as_str())
            || !expected_ids.contains(pair.incoming.as_str())
        {
            return Err("pair references an unknown track id".into());
        }
        if pair.expected_transition_kind != "BeatMatched" {
            return Err("real corpus pairs must require BeatMatched".into());
        }
        if pair.outgoing == pair.incoming {
            self_pairs.insert(pair.outgoing.as_str());
        } else {
            cross_pairs += 1;
        }
    }
    if self_pairs != expected_ids || cross_pairs < 2 {
        return Err("corpus must cover every self-pair and at least two cross-pairs".into());
    }
    let thresholds = manifest.thresholds;
    if thresholds.min_kick_coverage != 0.65
        || thresholds.min_matched_kicks != 8
        || thresholds.kick_phase_p95_ms != 25.0
        || thresholds.kick_phase_max_ms != 35.0
        || thresholds.kick_handoff_ms != 25.0
        || thresholds.minimum_rms_gap_ratio != 0.35
        || thresholds.silence_window_ms != 250
        || thresholds.silence_threshold_dbfs != -45.0
        || thresholds.maximum_sample_peak_dbfs != -1.0
        || thresholds.maximum_true_peak_dbtp != -1.0
    {
        return Err("corpus thresholds differ from the agreed release gate".into());
    }
    Ok(())
}

#[derive(Clone)]
struct PreparedTrack {
    request: TrackRequest,
    analysis: TrackAnalysis,
    source_hash_available: bool,
}

fn stage_deadline(fixture_deadline: Instant, stage_timeout: Duration) -> Instant {
    (Instant::now() + stage_timeout).min(fixture_deadline)
}

fn retryable_acquisition_code(code: &str) -> bool {
    matches!(
        code,
        "http_401"
            | "http_403"
            | "http_410"
            | "source_request_failed"
            | "source_body_failed"
            | "source_length_mismatch"
            | "range_requires_206"
            | "range_missing_content_range"
            | "range_invalid_content_range"
            | "range_content_range_mismatch"
            | "range_body_length_mismatch"
            | "range_short_nonfinal"
            | "range_full_body_length_missing"
            | "range_full_body_length_invalid"
            | "range_full_body_length_mismatch"
            | "hash_timeout"
    )
}

async fn prepare_track(
    fixture: &TrackFixture,
    resolver: &MediaResolver,
    runtime: &SongbirdRuntime,
    http: &Client,
) -> (Option<PreparedTrack>, TrackReport) {
    let fixture_deadline = Instant::now() + FIXTURE_TOTAL_TIMEOUT;
    let mut force_fresh = false;
    let mut all_attempts = Vec::new();
    let mut last_track = None;
    let mut last_report = TrackReport::failed(fixture, Vec::new(), "acquisition_failed");

    for cycle in 1..=MAX_ACQUISITION_CYCLES {
        let (track, mut report, retryable) = prepare_track_cycle(
            fixture,
            resolver,
            runtime,
            http,
            cycle,
            force_fresh,
            fixture_deadline,
        )
        .await;
        if track.is_some() {
            last_track = track.clone();
        }
        all_attempts.append(&mut report.extraction_attempts);
        report.extraction_attempts = all_attempts.clone();
        last_report = report;

        if !retryable || cycle == MAX_ACQUISITION_CYCLES || Instant::now() >= fixture_deadline {
            return (track.or(last_track), last_report);
        }

        force_fresh = true;
        all_attempts.push(AttemptReport {
            stage: "request_refresh".to_owned(),
            attempts: cycle + 1,
            ok: true,
            elapsed_ms: 0.0,
            error_code: Some("fresh_resolve_cycle".to_owned()),
        });
        eprintln!(
            "automix acquisition_refresh fixture={} cycle={} next_cycle={}",
            fixture.id,
            cycle,
            cycle + 1
        );
    }
    (last_track, last_report)
}

async fn prepare_track_cycle(
    fixture: &TrackFixture,
    resolver: &MediaResolver,
    runtime: &SongbirdRuntime,
    http: &Client,
    cycle: usize,
    force_fresh: bool,
    fixture_deadline: Instant,
) -> (Option<PreparedTrack>, TrackReport, bool) {
    let mut attempts = Vec::new();
    let started = stage_start("fixture", &fixture.id, "resolve");
    let resolve_deadline = stage_deadline(fixture_deadline, RESOLVE_STAGE_TIMEOUT);
    let request = match run_stage_until(
        async {
            if force_fresh {
                resolver.resolve_fresh(&fixture.url).await
            } else {
                resolver.resolve(&fixture.url).await
            }
        },
        resolve_deadline,
        "resolve_timeout",
    )
    .await
    {
        Ok(Ok(request)) => {
            if let Err(code) = validate_fixture_identity(fixture, &request) {
                stage_end("fixture", &fixture.id, "resolve", started, code);
                attempts.push(AttemptReport::failure("resolve", cycle, started, code));
                let mut report = TrackReport::failed(fixture, attempts, code);
                report.resolved_title = Some(request.metadata.title.to_string());
                report.resolved_video_id = youtube_video_id(request.canonical_url.as_ref());
                report.provider_id = Some(request.provider_id.to_string());
                report.canonical_key = Some(request.canonical_key.to_string());
                return (None, report, false);
            }
            stage_end("fixture", &fixture.id, "resolve", started, "ok");
            attempts.push(AttemptReport::success("resolve", cycle, started));
            request
        }
        Ok(Err(_)) => {
            stage_end("fixture", &fixture.id, "resolve", started, "resolve_failed");
            attempts.push(AttemptReport::failure(
                "resolve",
                cycle,
                started,
                "resolve_failed",
            ));
            return (
                None,
                TrackReport::failed(fixture, attempts, "resolve_failed"),
                true,
            );
        }
        Err(timeout) => {
            stage_end("fixture", &fixture.id, "resolve", started, timeout.code);
            attempts.push(AttemptReport::failure(
                "resolve",
                cycle,
                started,
                timeout.code,
            ));
            return (
                None,
                TrackReport::failed(fixture, attempts, timeout.code),
                true,
            );
        }
    };

    let started = stage_start("fixture", &fixture.id, "prepare");
    let prepare_deadline = stage_deadline(fixture_deadline, PREPARE_STAGE_TIMEOUT);
    let request = match run_stage_until(
        async {
            if force_fresh {
                resolver.prepare_playback_fresh(&request).await
            } else {
                resolver.prepare_playback(&request).await
            }
        },
        prepare_deadline,
        "prepare_timeout",
    )
    .await
    {
        Ok(Ok(request)) => {
            stage_end("fixture", &fixture.id, "prepare", started, "ok");
            attempts.push(AttemptReport::success("prepare", cycle, started));
            request
        }
        Ok(Err(_)) => {
            stage_end("fixture", &fixture.id, "prepare", started, "prepare_failed");
            attempts.push(AttemptReport::failure(
                "prepare",
                cycle,
                started,
                "prepare_failed",
            ));
            return (
                None,
                TrackReport::failed(fixture, attempts, "prepare_failed"),
                true,
            );
        }
        Err(timeout) => {
            stage_end("fixture", &fixture.id, "prepare", started, timeout.code);
            attempts.push(AttemptReport::failure(
                "prepare",
                cycle,
                started,
                timeout.code,
            ));
            return (
                None,
                TrackReport::failed(fixture, attempts, timeout.code),
                true,
            );
        }
    };

    let started = stage_start("fixture", &fixture.id, "content_hash");
    let hash_deadline = stage_deadline(fixture_deadline, CONTENT_HASH_STAGE_TIMEOUT);
    let acquisition = match run_stage_until(
        hash_prepared_source_until(http, &request, Some(hash_deadline)),
        hash_deadline,
        "hash_timeout",
    )
    .await
    {
        Ok(acquisition) => acquisition,
        Err(timeout) => AcquisitionOutcome {
            ok: false,
            requests: 0,
            digest: None,
            error_code: Some(timeout.code.to_owned()),
        },
    };
    let acquisition_code = acquisition
        .error_code
        .as_deref()
        .unwrap_or(if acquisition.ok { "ok" } else { "hash_failed" });
    stage_end(
        "fixture",
        &fixture.id,
        "content_hash",
        started,
        acquisition_code,
    );
    attempts.push(if acquisition.ok {
        AttemptReport::success("content_hash", acquisition.requests.max(cycle), started)
    } else {
        AttemptReport::failure(
            "content_hash",
            acquisition.requests,
            started,
            acquisition_code,
        )
    });

    let started = stage_start("fixture", &fixture.id, "analyze");
    let analyze_deadline = stage_deadline(fixture_deadline, ANALYZE_STAGE_TIMEOUT);
    let analysis_result = run_stage_until(
        async {
            if force_fresh {
                runtime.analyze_track_with_backend_fresh(&request).await
            } else {
                runtime.analyze_track_with_backend(&request).await
            }
        },
        analyze_deadline,
        "analyze_timeout",
    )
    .await;
    let (analysis, analysis_backend, analysis_code) = match analysis_result {
        Ok(Some(outcome)) => {
            let code = if outcome.backend.is_fresh_neural() {
                "ok"
            } else {
                "analysis_backend_not_neural"
            };
            (Some(outcome.analysis), Some(outcome.backend), code)
        }
        Ok(None) => (None, None, "analysis_unavailable"),
        Err(timeout) => (None, None, timeout.code),
    };
    stage_end("fixture", &fixture.id, "analyze", started, analysis_code);
    attempts.push(
        if analysis_backend.is_some_and(AnalysisBackend::is_fresh_neural) {
            AttemptReport::success("analyze", cycle, started)
        } else {
            AttemptReport::failure("analyze", cycle, started, analysis_code)
        },
    );
    let Some(analysis) = analysis else {
        let mut report = TrackReport::failed(fixture, attempts, analysis_code);
        report.resolved_title = Some(request.metadata.title.to_string());
        report.resolved_video_id = youtube_video_id(request.canonical_url.as_ref());
        report.provider_id = Some(request.provider_id.to_string());
        report.canonical_key = Some(request.canonical_key.to_string());
        report.source = acquisition.digest;
        return (None, report, true);
    };

    let analysis_backend =
        analysis_backend.expect("analysis backend is present when analysis is present");
    let neural_backend = analysis_backend.is_fresh_neural();
    let ready = strict_analysis_passes(acquisition.ok, Some(analysis_backend));
    let failure_code = if !acquisition.ok {
        Some("source_hash_failed".to_owned())
    } else if !neural_backend {
        Some("analysis_backend_not_neural".to_owned())
    } else {
        None
    };
    let report = TrackReport {
        id: fixture.id.clone(),
        expected_title: fixture.title.clone(),
        genre: fixture.genre.clone(),
        url: fixture.url.clone(),
        resolved_title: Some(request.metadata.title.to_string()),
        resolved_video_id: youtube_video_id(request.canonical_url.as_ref()),
        provider_id: Some(request.provider_id.to_string()),
        canonical_key: Some(request.canonical_key.to_string()),
        ready,
        failure_code,
        analysis_backend: Some(analysis_backend.as_str().to_owned()),
        analysis_backend_reason: Some(analysis_backend_reason(analysis_backend).to_owned()),
        extraction_attempts: attempts,
        source: acquisition.digest,
        analysis: Some(AnalysisReport::from(&analysis)),
    };
    (
        Some(PreparedTrack {
            request,
            analysis,
            source_hash_available: acquisition.ok,
        }),
        report,
        !acquisition.ok
            && acquisition
                .error_code
                .as_deref()
                .is_some_and(retryable_acquisition_code),
    )
}

fn corpus_http_client() -> Result<Client, AnyError> {
    Ok(Client::builder()
        .user_agent("wotoha-automix-corpus/1")
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(45))
        .redirect(Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 {
                attempt.error("too many redirects")
            } else if is_allowed_runtime_redirect_url(attempt.url().as_str()) {
                attempt.follow()
            } else {
                attempt.error("redirect host is not allowlisted")
            }
        }))
        .build()?)
}

#[derive(Debug)]
struct AcquisitionOutcome {
    ok: bool,
    requests: usize,
    digest: Option<SourceDigestReport>,
    error_code: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
struct SourceDigestReport {
    mode: String,
    sha256: String,
    bytes: u64,
}

#[cfg(test)]
async fn hash_prepared_source(http: &Client, request: &TrackRequest) -> AcquisitionOutcome {
    hash_prepared_source_until(http, request, None).await
}

async fn hash_prepared_source_until(
    http: &Client,
    request: &TrackRequest,
    deadline: Option<Instant>,
) -> AcquisitionOutcome {
    let PreparedSource::Http {
        stream_url,
        headers,
        content_length,
        range_chunk_size,
        range_mode,
        ..
    } = &request.prepared
    else {
        return AcquisitionOutcome {
            ok: false,
            requests: 0,
            digest: None,
            error_code: Some("hls_content_hash_unsupported".to_owned()),
        };
    };
    if content_length.is_some_and(|length| length == 0 || length > MAX_SOURCE_BYTES) {
        return AcquisitionOutcome {
            ok: false,
            requests: 0,
            digest: None,
            error_code: Some(
                if content_length == &Some(0) {
                    "empty_source"
                } else {
                    "source_too_large"
                }
                .to_owned(),
            ),
        };
    }
    if range_chunk_size.is_some_and(|size| size == 0) {
        return AcquisitionOutcome {
            ok: false,
            requests: 0,
            digest: None,
            error_code: Some("invalid_range_chunk_size".to_owned()),
        };
    }
    let headers = match request_headers(headers) {
        Ok(headers) => headers,
        Err(code) => {
            return AcquisitionOutcome {
                ok: false,
                requests: 0,
                digest: None,
                error_code: Some(code.to_owned()),
            };
        }
    };
    let ranged = range_chunk_size.is_some();
    // A non-ranged response is accepted only when the resolver supplied a length we can
    // authenticate.  Without it, a successful 200 could be a truncated or unrelated body.
    if !ranged && content_length.is_none() {
        return AcquisitionOutcome {
            ok: false,
            requests: 0,
            digest: None,
            error_code: Some("source_length_unknown".to_owned()),
        };
    }
    let mut hasher = Sha256::new();
    let mut offset = 0_u64;
    let mut total_length = *content_length;
    let mut requests = 0;
    let chunk_size = range_chunk_size
        .filter(|size| *size > 0)
        .or(total_length)
        .unwrap_or(MAX_SOURCE_BYTES);

    loop {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return AcquisitionOutcome {
                ok: false,
                requests,
                digest: None,
                error_code: Some("hash_timeout".to_owned()),
            };
        }
        if offset >= total_length.unwrap_or(u64::MAX) || offset >= MAX_SOURCE_BYTES {
            break;
        }
        let remaining = total_length.map_or(MAX_SOURCE_BYTES - offset, |length| length - offset);
        let requested = remaining.min(chunk_size).min(MAX_SOURCE_BYTES - offset);
        if requested == 0 {
            break;
        }
        let range_end = offset.saturating_add(requested).saturating_sub(1);
        let range = format!("bytes={offset}-{range_end}");
        let mut last_code = "source_request_failed";
        let mut body = None;
        for _ in 0..HASH_REQUEST_RETRIES {
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                last_code = "hash_timeout";
                break;
            }
            requests += 1;
            let request_url =
                if range_chunk_size.is_some() && *range_mode == PreparedRangeMode::QueryParam {
                    match Url::parse(stream_url) {
                        Ok(mut url) => {
                            url.query_pairs_mut()
                                .append_pair("range", range.trim_start_matches("bytes="));
                            url
                        }
                        Err(_) => {
                            last_code = "invalid_prepared_url";
                            break;
                        }
                    }
                } else {
                    match Url::parse(stream_url) {
                        Ok(url) => url,
                        Err(_) => {
                            last_code = "invalid_prepared_url";
                            break;
                        }
                    }
                };
            if !hash_url_allowed(&request_url) {
                last_code = "source_url_not_https";
                break;
            }
            let mut builder = http.get(request_url).headers(headers.clone());
            if range_chunk_size.is_some() && *range_mode == PreparedRangeMode::Header {
                builder = builder.header(RANGE, &range);
            }
            let send_result = match deadline {
                Some(deadline) => match tokio::time::timeout_at(
                    tokio::time::Instant::from_std(deadline),
                    builder.send(),
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => {
                        last_code = "hash_timeout";
                        break;
                    }
                },
                None => builder.send().await,
            };
            match send_result {
                Ok(response) => {
                    if !hash_url_allowed(response.url()) {
                        last_code = "source_url_not_https";
                        continue;
                    }
                    let status = response.status();
                    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
                        || status == StatusCode::GONE
                    {
                        // A signed provider URL is not made valid by retrying it.  Return the
                        // acquisition code immediately so the caller can resolve a fresh URL.
                        last_code = http_error_code(status);
                        break;
                    }
                    let response_headers = response.headers().clone();
                    if !ranged {
                        if status != StatusCode::OK {
                            last_code = http_error_code(status);
                            continue;
                        }
                        let Some(expected) = total_length else {
                            last_code = "source_length_unknown";
                            continue;
                        };
                        if !full_body_media_type(&response_headers) {
                            last_code = "full_body_media_type_invalid";
                            continue;
                        }
                        if response
                            .content_length()
                            .is_some_and(|length| length != expected)
                        {
                            last_code = "source_length_mismatch";
                            continue;
                        }
                        match bounded_response_body(response, expected, deadline).await {
                            Ok(bytes) if bytes.len() as u64 == expected && !bytes.is_empty() => {
                                body = Some(bytes);
                                break;
                            }
                            Err(code) => last_code = code,
                            Ok(_) => last_code = "source_length_mismatch",
                        }
                        continue;
                    }

                    // Some CDNs ignore Range and return the complete object. Accept that only
                    // as an authenticated full-body representation; never append it to a
                    // partial hash or issue another range request afterward.
                    if status == StatusCode::OK {
                        let Some(full_length) = full_body_length(&response_headers) else {
                            last_code = "range_full_body_length_missing";
                            continue;
                        };
                        if full_length == 0 || full_length > MAX_SOURCE_BYTES {
                            last_code = "range_full_body_length_invalid";
                            continue;
                        }
                        if total_length.is_some_and(|expected| expected != full_length) {
                            last_code = "range_full_body_length_mismatch";
                            continue;
                        }
                        if !full_body_media_type(&response_headers) {
                            last_code = "range_full_body_media_type_invalid";
                            continue;
                        }
                        let bytes =
                            match bounded_response_body(response, full_length, deadline).await {
                                Ok(bytes) => bytes,
                                Err(code) => {
                                    last_code = code;
                                    continue;
                                }
                            };
                        if bytes.len() as u64 != full_length || bytes.is_empty() {
                            last_code = "range_full_body_length_mismatch";
                            continue;
                        }
                        return AcquisitionOutcome {
                            ok: true,
                            requests,
                            digest: Some(SourceDigestReport {
                                mode: "http-content".to_owned(),
                                sha256: format!("{:x}", Sha256::digest(&bytes)),
                                bytes: full_length,
                            }),
                            error_code: None,
                        };
                    }
                    if status != StatusCode::PARTIAL_CONTENT {
                        last_code = http_error_code(status);
                        continue;
                    }
                    let Some(content_range) = response_headers.get(CONTENT_RANGE) else {
                        last_code = "range_missing_content_range";
                        continue;
                    };
                    let content_range = match content_range.to_str() {
                        Ok(value) => value,
                        Err(_) => {
                            last_code = "range_invalid_content_range";
                            continue;
                        }
                    };
                    let (start, end, total) = match parse_content_range(content_range) {
                        Ok(range) => range,
                        Err(code) => {
                            last_code = code;
                            continue;
                        }
                    };
                    if start != offset
                        || total_length.is_some_and(|expected| expected != total)
                        || total > MAX_SOURCE_BYTES
                        || end < start
                    {
                        last_code = "range_content_range_mismatch";
                        continue;
                    }
                    if response
                        .content_length()
                        .is_some_and(|length| length > requested || length > MAX_SOURCE_BYTES)
                    {
                        last_code = "source_length_exceeded";
                        continue;
                    }
                    let bytes = match bounded_response_body(response, requested, deadline).await {
                        Ok(bytes) => bytes,
                        Err(code) => {
                            last_code = code;
                            continue;
                        }
                    };
                    let body_length = bytes.len() as u64;
                    let expected_body_length = end - start + 1;
                    let final_range = end + 1 == total;
                    if bytes.is_empty()
                        || body_length != expected_body_length
                        || body_length > requested
                        || (!final_range && body_length != requested)
                        || offset.saturating_add(body_length) > total
                    {
                        last_code = if body_length > MAX_SOURCE_BYTES {
                            "source_length_exceeded"
                        } else if !final_range && body_length < requested {
                            "range_short_nonfinal"
                        } else {
                            "range_body_length_mismatch"
                        };
                        continue;
                    }
                    total_length = Some(total);
                    body = Some(bytes);
                    break;
                }
                Err(_) => last_code = "source_request_failed",
            }
        }
        let Some(body) = body else {
            return AcquisitionOutcome {
                ok: false,
                requests,
                digest: None,
                error_code: Some(last_code.to_owned()),
            };
        };
        let body_length = body.len() as u64;
        if offset.saturating_add(body_length) > MAX_SOURCE_BYTES
            || total_length.is_some_and(|length| offset.saturating_add(body_length) > length)
        {
            return AcquisitionOutcome {
                ok: false,
                requests,
                digest: None,
                error_code: Some("source_length_exceeded".to_owned()),
            };
        }
        hasher.update(&body);
        offset += body_length;
        if !ranged {
            break;
        }
    }
    if total_length.is_none() || total_length.is_some_and(|expected| expected != offset) {
        return AcquisitionOutcome {
            ok: false,
            requests,
            digest: None,
            error_code: Some("source_length_mismatch".to_owned()),
        };
    }
    AcquisitionOutcome {
        ok: true,
        requests,
        digest: Some(SourceDigestReport {
            mode: "http-content".to_owned(),
            sha256: format!("{:x}", hasher.finalize()),
            bytes: offset,
        }),
        error_code: None,
    }
}

async fn bounded_response_body(
    mut response: reqwest::Response,
    limit: u64,
    deadline: Option<Instant>,
) -> Result<Vec<u8>, &'static str> {
    let mut body = Vec::new();
    loop {
        let chunk = match deadline {
            Some(deadline) => {
                tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), response.chunk())
                    .await
                    .map_err(|_| "hash_timeout")?
                    .map_err(|_| "source_body_failed")?
            }
            None => response.chunk().await.map_err(|_| "source_body_failed")?,
        };
        let Some(chunk) = chunk else {
            break;
        };
        if body.len() as u64 + chunk.len() as u64 > limit {
            return Err("source_length_exceeded");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn parse_content_range(value: &str) -> Result<(u64, u64, u64), &'static str> {
    let value = value.trim();
    let mut parts = value.split_ascii_whitespace();
    if parts.next() != Some("bytes") {
        return Err("range_invalid_content_range");
    }
    let range_and_total = parts.next().ok_or("range_invalid_content_range")?;
    if parts.next().is_some() {
        return Err("range_invalid_content_range");
    }
    let (range, total) = range_and_total
        .split_once('/')
        .ok_or("range_invalid_content_range")?;
    let (start, end) = range.split_once('-').ok_or("range_invalid_content_range")?;
    let start = start
        .parse::<u64>()
        .map_err(|_| "range_invalid_content_range")?;
    let end = end
        .parse::<u64>()
        .map_err(|_| "range_invalid_content_range")?;
    let total = total
        .parse::<u64>()
        .map_err(|_| "range_invalid_content_range")?;
    if start > end || total == 0 || end >= total {
        return Err("range_invalid_content_range");
    }
    Ok((start, end, total))
}

fn request_headers(headers: &[wotoha_core::PreparedHeader]) -> Result<HeaderMap, &'static str> {
    let mut result = HeaderMap::new();
    for header in headers {
        let name = HeaderName::from_bytes(header.name.as_bytes()).map_err(|_| "invalid_header")?;
        let value = HeaderValue::from_str(header.value.as_ref()).map_err(|_| "invalid_header")?;
        result.insert(name, value);
    }
    Ok(result)
}

fn http_error_code(status: StatusCode) -> &'static str {
    match status.as_u16() {
        401 => "http_401",
        403 => "http_403",
        410 => "http_410",
        404 => "http_404",
        429 => "http_429",
        code if (500..=599).contains(&code) => "http_5xx",
        _ => "http_error",
    }
}

fn hash_url_allowed(url: &Url) -> bool {
    if url.scheme() == "https" {
        return true;
    }
    #[cfg(test)]
    {
        return url.scheme() == "http"
            && matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1"));
    }
    #[cfg(not(test))]
    {
        false
    }
}

fn full_body_media_type(headers: &HeaderMap) -> bool {
    let Some(value) = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let media_type = value
        .split(';')
        .next()
        .map(str::trim)
        .unwrap_or_default()
        .to_ascii_lowercase();
    media_type.starts_with("audio/")
        || media_type.starts_with("video/")
        || matches!(
            media_type.as_str(),
            "application/octet-stream" | "application/mp4" | "application/ogg"
        )
}

fn full_body_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
}

async fn evaluate_pair(
    fixture: &PairFixture,
    tracks: &HashMap<String, PreparedTrack>,
    runtime: &SongbirdRuntime,
    automix: &AutoMixConfig,
    loudness: &LoudnessConfig,
    thresholds: &Thresholds,
    preview_dir: &Path,
) -> PairReport {
    let mut failures = Vec::new();
    let mut attempts = Vec::new();
    let pair_id = format!("{}->{}", fixture.outgoing, fixture.incoming);
    let plan_started = stage_start("pair", &pair_id, "plan");
    let Some(outgoing) = tracks.get(&fixture.outgoing) else {
        failures.push("outgoing_unavailable".to_owned());
        stage_end(
            "pair",
            &pair_id,
            "plan",
            plan_started,
            "outgoing_unavailable",
        );
        return PairReport::unavailable(fixture, failures);
    };
    let Some(incoming) = tracks.get(&fixture.incoming) else {
        failures.push("incoming_unavailable".to_owned());
        stage_end(
            "pair",
            &pair_id,
            "plan",
            plan_started,
            "incoming_unavailable",
        );
        return PairReport::unavailable(fixture, failures);
    };
    if !outgoing.source_hash_available {
        failures.push("outgoing_source_hash_unavailable".to_owned());
    }
    if !incoming.source_hash_available {
        failures.push("incoming_source_hash_unavailable".to_owned());
    }
    let outgoing_gain = loudness_normalization_gain(loudness, Some(&outgoing.analysis));
    let incoming_gain = loudness_normalization_gain(loudness, Some(&incoming.analysis));
    let guarded = plan_guarded_transition_with_base_gains(
        &outgoing.analysis,
        &incoming.analysis,
        automix,
        outgoing_gain,
        incoming_gain,
    );
    let decision = format!(
        "{:?}",
        explain_beatmatch_decision(&outgoing.analysis, &incoming.analysis, automix, &guarded,)
    );
    let plan = PlanReport::from_guarded(&guarded);
    stage_end("pair", &pair_id, "plan", plan_started, "ok");
    if plan.kind != fixture.expected_transition_kind {
        failures.push("transition_kind".to_owned());
    }

    let started = stage_start("pair", &pair_id, "render");
    let render_deadline = started + PAIR_RENDER_STAGE_TIMEOUT;
    // The runtime's async input preparation is deadline-bounded here.  Its API then enters a
    // synchronous renderer; Tokio cannot preempt that portion, which is intentionally limited to
    // the finite analysis-selected segments and the runtime client's bounded stream reads.
    let preview = match run_stage_until(
        runtime.render_automix_preview(
            &outgoing.request,
            &incoming.request,
            &outgoing.analysis,
            &incoming.analysis,
            automix,
            loudness,
        ),
        render_deadline,
        "render_timeout",
    )
    .await
    {
        Ok(Ok(preview)) => {
            stage_end("pair", &pair_id, "render", started, "ok");
            Ok(preview)
        }
        Ok(Err(_error)) => {
            stage_end("pair", &pair_id, "render", started, "render_failed");
            Err("render_failed")
        }
        Err(timeout) => {
            stage_end("pair", &pair_id, "render", started, timeout.code);
            Err(timeout.code)
        }
    };
    let (mut pcm, preview_path, render_model) = match preview {
        Ok(preview) => {
            attempts.push(AttemptReport::success("render_preview", 1, started));
            let path = preview_dir.join(format!(
                "{}--{}.wav",
                safe_component(&fixture.outgoing),
                safe_component(&fixture.incoming)
            ));
            let write_ok = fs::write(&path, &preview.wav).is_ok();
            if !write_ok {
                failures.push("preview_write_failed".to_owned());
            }
            let expected_kicks =
                expected_overlap_beats(&outgoing.analysis, &incoming.analysis, &preview.plan);
            let remeasure_started = stage_start("pair", &pair_id, "remeasure");
            let measured = measure_wav_with_overlap(
                &preview.wav,
                outgoing.analysis.bpm,
                expected_kicks,
                thresholds,
            )
            .ok();
            stage_end(
                "pair",
                &pair_id,
                "remeasure",
                remeasure_started,
                if measured.is_some() {
                    "ok"
                } else {
                    "remeasure_failed"
                },
            );
            if measured.is_none() {
                failures.push("pcm_measurement_failed".to_owned());
            }
            (
                measured,
                write_ok.then(|| path.display().to_string()),
                Some(RenderModelReport {
                    start_rms_dbfs: preview.render_metrics.start_rms_dbfs,
                    mid_rms_dbfs: preview.render_metrics.mid_rms_dbfs,
                    end_rms_dbfs: preview.render_metrics.end_rms_dbfs,
                    quietest_window_rms_dbfs: preview.render_metrics.quietest_window_rms_dbfs,
                    quietest_to_edge_ratio: preview.render_metrics.quietest_to_edge_ratio,
                    sample_peak_dbfs: preview.render_metrics.sample_peak_dbfs,
                    issues: preview
                        .render_issues
                        .iter()
                        .map(|issue| format!("{issue:?}"))
                        .collect(),
                }),
            )
        }
        Err(error_code) => {
            attempts.push(AttemptReport::failure(
                "render_preview",
                1,
                started,
                error_code,
            ));
            failures.push(error_code.to_owned());
            (None, None, None)
        }
    };
    if let Some(pcm) = &mut pcm {
        apply_pcm_thresholds(pcm, thresholds, &mut failures);
    }
    failures.sort();
    failures.dedup();
    PairReport {
        outgoing: fixture.outgoing.clone(),
        incoming: fixture.incoming.clone(),
        expected_transition_kind: fixture.expected_transition_kind.clone(),
        beatmatch_decision: Some(decision),
        passed: failures.is_empty(),
        failures,
        extraction_attempts: attempts,
        normalization: Some(NormalizationReport {
            outgoing_gain,
            incoming_gain,
        }),
        plan: Some(plan),
        render_model,
        rendered_pcm: pcm,
        preview_path,
    }
}

fn safe_component(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn apply_pcm_thresholds(pcm: &mut PcmReport, thresholds: &Thresholds, failures: &mut Vec<String>) {
    match &mut pcm.kick_phase {
        Some(kick) => {
            if kick.expected_beats == 0 {
                kick.issues
                    .push("kick_expected_beats_unavailable".to_owned());
                failures.push("kick_expected_beats_unavailable".to_owned());
            }
            if kick.matched_beats < thresholds.min_matched_kicks {
                kick.issues.push("kick_matched_beats".to_owned());
                failures.push("kick_matched_beats".to_owned());
            }
            if !kick.beat_coverage.is_finite() || kick.beat_coverage < thresholds.min_kick_coverage
            {
                kick.issues.push("kick_coverage".to_owned());
                failures.push("kick_coverage".to_owned());
            }
            if kick.p95_ms > thresholds.kick_phase_p95_ms {
                kick.issues.push("kick_phase_p95".to_owned());
                failures.push("kick_phase_p95".to_owned());
            }
            if kick.max_ms > thresholds.kick_phase_max_ms {
                kick.issues.push("kick_phase_max".to_owned());
                failures.push("kick_phase_max".to_owned());
            }
            if kick.handoff_ms > thresholds.kick_handoff_ms {
                kick.issues.push("kick_handoff".to_owned());
                failures.push("kick_handoff".to_owned());
            }
            kick.issues.sort();
            kick.issues.dedup();
        }
        None => failures.push("kick_phase_unavailable".to_owned()),
    }
    if pcm.rms_500ms.quietest_to_edge_ratio < thresholds.minimum_rms_gap_ratio {
        failures.push("rms_gap_ratio".to_owned());
    }
    if pcm.silence.windows_detected > 0 {
        failures.push("silence_250ms".to_owned());
    }
    if pcm.sample_peak_dbfs > thresholds.maximum_sample_peak_dbfs {
        failures.push("sample_peak".to_owned());
    }
    if pcm.true_peak_dbtp > thresholds.maximum_true_peak_dbtp {
        failures.push("true_peak".to_owned());
    }
}

fn automix_config() -> AutoMixConfig {
    AutoMixConfig {
        enabled: true,
        crossfade: Duration::from_secs(8),
        max_tempo_adjustment: 0.06,
        min_beat_confidence: 0.7,
    }
}

fn loudness_config() -> LoudnessConfig {
    LoudnessConfig {
        enabled: true,
        target_lufs: -16.0,
        max_boost_db: 6.0,
        true_peak_ceiling_dbtp: -2.0,
    }
}

#[derive(Debug, Serialize)]
struct CorpusReport {
    schema_version: u32,
    passed: bool,
    manifest_sha256: String,
    run_directory: String,
    config: ConfigReport,
    thresholds: Thresholds,
    tracks: Vec<TrackReport>,
    pairs: Vec<PairReport>,
}

#[derive(Debug, Serialize)]
struct ConfigReport {
    crossfade_ms: u128,
    max_tempo_adjustment: f32,
    min_beat_confidence: f32,
    target_lufs: f32,
    true_peak_ceiling_dbtp: f32,
}

impl ConfigReport {
    fn from_configs(automix: &AutoMixConfig, loudness: &LoudnessConfig) -> Self {
        Self {
            crossfade_ms: automix.crossfade.as_millis(),
            max_tempo_adjustment: automix.max_tempo_adjustment,
            min_beat_confidence: automix.min_beat_confidence,
            target_lufs: loudness.target_lufs,
            true_peak_ceiling_dbtp: loudness.true_peak_ceiling_dbtp,
        }
    }
}

#[derive(Debug, Serialize)]
struct TrackReport {
    id: String,
    expected_title: String,
    genre: String,
    url: String,
    resolved_title: Option<String>,
    resolved_video_id: Option<String>,
    provider_id: Option<String>,
    canonical_key: Option<String>,
    ready: bool,
    failure_code: Option<String>,
    analysis_backend: Option<String>,
    analysis_backend_reason: Option<String>,
    extraction_attempts: Vec<AttemptReport>,
    source: Option<SourceDigestReport>,
    analysis: Option<AnalysisReport>,
}

impl TrackReport {
    fn failed(fixture: &TrackFixture, attempts: Vec<AttemptReport>, code: &str) -> Self {
        Self {
            id: fixture.id.clone(),
            expected_title: fixture.title.clone(),
            genre: fixture.genre.clone(),
            url: fixture.url.clone(),
            resolved_title: None,
            resolved_video_id: None,
            provider_id: None,
            canonical_key: None,
            ready: false,
            failure_code: Some(code.to_owned()),
            analysis_backend: None,
            analysis_backend_reason: None,
            extraction_attempts: attempts,
            source: None,
            analysis: None,
        }
    }
}

#[derive(Debug, Serialize)]
struct AnalysisReport {
    duration_ms: u128,
    audible_start_ms: u128,
    audible_end_ms: u128,
    intro_end_ms: Option<u128>,
    outro_start_ms: Option<u128>,
    bpm: Option<f32>,
    beat_confidence: f32,
    trusted_kick_coverage: f32,
    first_beat_ms: Option<u128>,
    first_downbeat_ms: Option<u128>,
    downbeat_confidence: f32,
    integrated_lufs: Option<f32>,
    true_peak_dbtp: Option<f32>,
}

impl From<&TrackAnalysis> for AnalysisReport {
    fn from(analysis: &TrackAnalysis) -> Self {
        Self {
            duration_ms: analysis.duration.as_millis(),
            audible_start_ms: analysis.audible_start.as_millis(),
            audible_end_ms: analysis.audible_end.as_millis(),
            intro_end_ms: analysis.intro_end.map(|value| value.as_millis()),
            outro_start_ms: analysis.outro_start.map(|value| value.as_millis()),
            bpm: analysis.bpm,
            beat_confidence: analysis.beat_confidence,
            trusted_kick_coverage: analysis.trusted_kick_coverage(),
            first_beat_ms: analysis.first_beat.map(|value| value.as_millis()),
            first_downbeat_ms: analysis.first_downbeat.map(|value| value.as_millis()),
            downbeat_confidence: analysis.downbeat_confidence,
            integrated_lufs: analysis.integrated_lufs,
            true_peak_dbtp: analysis.true_peak_dbtp,
        }
    }
}

fn analysis_backend_reason(backend: AnalysisBackend) -> &'static str {
    match backend {
        AnalysisBackend::Neural => "fresh_neural_inference",
        AnalysisBackend::ClassicalPermanentIneligible => "fresh_classical_permanent_ineligible",
        AnalysisBackend::ClassicalTransientFailure => "fresh_classical_transient_fallback",
        AnalysisBackend::CachedNeural => "analysis_cache_hit",
        AnalysisBackend::CachedClassicalPermanentIneligible => "classical_cache_hit",
        AnalysisBackend::CachedClassicalTransientFailure => {
            "classical_transient_fallback_cache_hit"
        }
    }
}

fn strict_analysis_passes(acquisition_ok: bool, backend: Option<AnalysisBackend>) -> bool {
    acquisition_ok && backend.is_some_and(AnalysisBackend::is_fresh_neural)
}

#[derive(Clone, Debug, Serialize)]
struct AttemptReport {
    stage: String,
    attempts: usize,
    ok: bool,
    elapsed_ms: f64,
    error_code: Option<String>,
}

impl AttemptReport {
    fn success(stage: &str, attempts: usize, started: Instant) -> Self {
        Self {
            stage: stage.to_owned(),
            attempts,
            ok: true,
            elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
            error_code: None,
        }
    }

    fn failure(stage: &str, attempts: usize, started: Instant, error_code: &str) -> Self {
        Self {
            stage: stage.to_owned(),
            attempts,
            ok: false,
            elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
            error_code: Some(error_code.to_owned()),
        }
    }
}

#[derive(Debug, Serialize)]
struct PairReport {
    outgoing: String,
    incoming: String,
    expected_transition_kind: String,
    beatmatch_decision: Option<String>,
    passed: bool,
    failures: Vec<String>,
    extraction_attempts: Vec<AttemptReport>,
    normalization: Option<NormalizationReport>,
    plan: Option<PlanReport>,
    render_model: Option<RenderModelReport>,
    rendered_pcm: Option<PcmReport>,
    preview_path: Option<String>,
}

impl PairReport {
    fn unavailable(fixture: &PairFixture, failures: Vec<String>) -> Self {
        Self {
            outgoing: fixture.outgoing.clone(),
            incoming: fixture.incoming.clone(),
            expected_transition_kind: fixture.expected_transition_kind.clone(),
            beatmatch_decision: None,
            passed: false,
            failures,
            extraction_attempts: Vec::new(),
            normalization: None,
            plan: None,
            render_model: None,
            rendered_pcm: None,
            preview_path: None,
        }
    }
}

#[derive(Debug, Serialize)]
struct NormalizationReport {
    outgoing_gain: f32,
    incoming_gain: f32,
}

#[derive(Debug, Serialize)]
struct PlanReport {
    kind: String,
    outgoing_start_ms: u128,
    incoming_start_ms: u128,
    duration_ms: u128,
    tempo_ratio: f32,
    tempo_end_ratio: f32,
    quality: QualityReport,
    rejected_kind: Option<String>,
    rejected_quality: Option<QualityReport>,
}

impl PlanReport {
    fn from_guarded(guarded: &GuardedTransitionPlan) -> Self {
        let plan = &guarded.plan;
        Self {
            kind: transition_kind(plan.kind).to_owned(),
            outgoing_start_ms: plan.outgoing_start.as_millis(),
            incoming_start_ms: plan.incoming_start.as_millis(),
            duration_ms: plan.duration.as_millis(),
            tempo_ratio: plan.incoming_tempo_ratio,
            tempo_end_ratio: plan
                .tempo_envelope
                .map_or(plan.incoming_tempo_ratio, |envelope| envelope.mix_end_speed),
            quality: QualityReport::from(&guarded.quality),
            rejected_kind: guarded
                .rejected_plan
                .as_ref()
                .map(|plan| transition_kind(plan.kind).to_owned()),
            rejected_quality: guarded.rejected_quality.as_ref().map(QualityReport::from),
        }
    }
}

fn transition_kind(kind: TransitionKind) -> &'static str {
    match kind {
        TransitionKind::BeatMatched => "BeatMatched",
        TransitionKind::Crossfade => "Crossfade",
        TransitionKind::Gapless => "Gapless",
    }
}

#[derive(Debug, Serialize)]
struct QualityReport {
    issues: Vec<String>,
    beat_pairs: usize,
    beat_phase_coverage: Option<f32>,
    beat_phase_max_ms: Option<u128>,
    beat_handoff_ms: Option<u128>,
    downbeat_pairs: usize,
    downbeat_phase_max_ms: Option<u128>,
    phrase_pairs: usize,
    phrase_phase_max_ms: Option<u128>,
    minimum_mix_energy_ratio: Option<f32>,
}

impl From<&AutoMixQualityReport> for QualityReport {
    fn from(quality: &AutoMixQualityReport) -> Self {
        Self {
            issues: quality
                .issues
                .iter()
                .map(|issue| format!("{issue:?}"))
                .collect(),
            beat_pairs: quality.beat_pairs_checked,
            beat_phase_coverage: quality.beat_phase_coverage,
            beat_phase_max_ms: quality.max_beat_phase_error.map(|value| value.as_millis()),
            beat_handoff_ms: quality
                .handoff_beat_phase_error
                .map(|value| value.as_millis()),
            downbeat_pairs: quality.downbeat_pairs_checked,
            downbeat_phase_max_ms: quality
                .max_downbeat_phase_error
                .map(|value| value.as_millis()),
            phrase_pairs: quality.phrase_pairs_checked,
            phrase_phase_max_ms: quality
                .max_phrase_phase_error
                .map(|value| value.as_millis()),
            minimum_mix_energy_ratio: quality.min_mix_energy_ratio,
        }
    }
}

#[derive(Debug, Serialize)]
struct RenderModelReport {
    start_rms_dbfs: f32,
    mid_rms_dbfs: f32,
    end_rms_dbfs: f32,
    quietest_window_rms_dbfs: f32,
    quietest_to_edge_ratio: f32,
    sample_peak_dbfs: f32,
    issues: Vec<String>,
}

#[derive(Debug, Serialize)]
struct PcmReport {
    wav_sha256: String,
    sample_rate: u32,
    channels: u16,
    duration_ms: f64,
    sample_peak_dbfs: f32,
    true_peak_dbtp: f32,
    kick_phase: Option<KickPhaseReport>,
    rms_500ms: RmsReport,
    silence: SilenceReport,
}

#[derive(Debug, Serialize)]
struct KickPhaseReport {
    expected_bpm: f32,
    candidate_onsets: usize,
    matched_beats: usize,
    expected_beats: usize,
    beat_coverage: f32,
    p95_ms: f32,
    max_ms: f32,
    handoff_ms: f32,
    issues: Vec<String>,
}

#[derive(Debug, Serialize)]
struct RmsReport {
    window_ms: u32,
    hop_ms: u32,
    windows: usize,
    quietest_dbfs: f32,
    edge_reference_dbfs: f32,
    quietest_to_edge_ratio: f32,
    consecutive_below_ratio_windows: usize,
}

#[derive(Debug, Serialize)]
struct SilenceReport {
    window_ms: u32,
    threshold_dbfs: f32,
    windows_detected: usize,
    longest_run_ms: u32,
}

struct WavPcm {
    sample_rate: u32,
    channels: u16,
    samples: Vec<f32>,
}

#[cfg(test)]
fn measure_wav(
    wav: &[u8],
    expected_bpm: Option<f32>,
    thresholds: &Thresholds,
) -> Result<PcmReport, AnyError> {
    measure_wav_internal(wav, expected_bpm, thresholds, None)
}

/// Measure a rendered overlap using only beats for which both source analyses provide a grid.
/// `None` is deliberately different from the legacy generated-PCM helper: it means that the
/// source grids were unavailable and therefore coverage must fail closed.
fn measure_wav_with_overlap(
    wav: &[u8],
    expected_bpm: Option<f32>,
    expected_beats: Option<usize>,
    thresholds: &Thresholds,
) -> Result<PcmReport, AnyError> {
    measure_wav_internal(wav, expected_bpm, thresholds, Some(expected_beats))
}

fn measure_wav_internal(
    wav: &[u8],
    expected_bpm: Option<f32>,
    thresholds: &Thresholds,
    strict_expected_beats: Option<Option<usize>>,
) -> Result<PcmReport, AnyError> {
    let pcm = decode_pcm16_wav(wav)?;
    let frames = pcm.samples.len() / usize::from(pcm.channels);
    let sample_peak = pcm
        .samples
        .iter()
        .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
    let true_peak = true_peak(&pcm)?;
    let rms = measure_rms(&pcm, thresholds.minimum_rms_gap_ratio)?;
    let silence = measure_silence(
        &pcm,
        thresholds.silence_window_ms,
        thresholds.silence_threshold_dbfs,
    )?;
    let expected_beats = strict_expected_beats.unwrap_or_else(|| {
        expected_bpm.map(|bpm| {
            if bpm.is_finite() && bpm > 0.0 {
                (frames as f32 / pcm.sample_rate as f32 / (60.0 / bpm)).ceil() as usize
            } else {
                0
            }
        })
    });
    let kick_phase =
        expected_bpm.and_then(|bpm| measure_kick_phase_with_expected(&pcm, bpm, expected_beats));
    Ok(PcmReport {
        wav_sha256: sha256_hex(wav),
        sample_rate: pcm.sample_rate,
        channels: pcm.channels,
        duration_ms: frames as f64 / f64::from(pcm.sample_rate) * 1000.0,
        sample_peak_dbfs: amplitude_dbfs(sample_peak),
        true_peak_dbtp: amplitude_dbfs(true_peak),
        kick_phase,
        rms_500ms: rms,
        silence,
    })
}

fn expected_overlap_beats(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    plan: &wotoha_core::automix::TransitionPlan,
) -> Option<usize> {
    if plan.duration.is_zero() {
        return Some(0);
    }
    if outgoing.bpm.is_none()
        || incoming.bpm.is_none()
        || outgoing.beat_markers.is_empty()
        || incoming.beat_markers.is_empty()
    {
        return None;
    }
    let outgoing_end = plan.outgoing_start.saturating_add(plan.duration);
    let incoming_duration = plan.tempo_envelope.map_or(plan.duration, |envelope| {
        envelope.source_elapsed(plan.duration)
    });
    let incoming_end = plan.incoming_start.saturating_add(incoming_duration);
    let outgoing_beats = outgoing
        .beat_markers
        .iter()
        .filter(|beat| **beat >= plan.outgoing_start && **beat <= outgoing_end)
        .count();
    let incoming_beats = incoming
        .beat_markers
        .iter()
        .filter(|beat| **beat >= plan.incoming_start && **beat <= incoming_end)
        .count();
    Some(outgoing_beats.min(incoming_beats))
}

fn decode_pcm16_wav(wav: &[u8]) -> Result<WavPcm, AnyError> {
    if wav.len() < 12 || &wav[..4] != b"RIFF" || &wav[8..12] != b"WAVE" {
        return Err("preview is not a RIFF/WAVE file".into());
    }
    let mut cursor = 12;
    let mut format = None;
    let mut data = None;
    while cursor + 8 <= wav.len() {
        let id = &wav[cursor..cursor + 4];
        let length = u32::from_le_bytes(wav[cursor + 4..cursor + 8].try_into()?) as usize;
        let start = cursor + 8;
        let end = start.checked_add(length).ok_or("WAV chunk overflow")?;
        if end > wav.len() {
            return Err("truncated WAV chunk".into());
        }
        if id == b"fmt " {
            format = Some(&wav[start..end]);
        } else if id == b"data" {
            data = Some(&wav[start..end]);
        }
        cursor = end + (length & 1);
    }
    let format = format.ok_or("WAV format chunk is missing")?;
    let data = data.ok_or("WAV data chunk is missing")?;
    if format.len() < 16 {
        return Err("WAV format chunk is truncated".into());
    }
    let encoding = u16::from_le_bytes(format[0..2].try_into()?);
    let channels = u16::from_le_bytes(format[2..4].try_into()?);
    let sample_rate = u32::from_le_bytes(format[4..8].try_into()?);
    let bits = u16::from_le_bytes(format[14..16].try_into()?);
    if encoding != 1 || channels == 0 || sample_rate == 0 || bits != 16 || data.len() % 2 != 0 {
        return Err("only non-empty PCM16 WAV previews are supported".into());
    }
    let samples = data
        .chunks_exact(2)
        .map(|sample| i16::from_le_bytes([sample[0], sample[1]]) as f32 / 32768.0)
        .collect::<Vec<_>>();
    if samples.is_empty() || samples.len() % usize::from(channels) != 0 {
        return Err("WAV PCM frame layout is invalid".into());
    }
    Ok(WavPcm {
        sample_rate,
        channels,
        samples,
    })
}

fn true_peak(pcm: &WavPcm) -> Result<f32, AnyError> {
    let mut analyzer = EbuR128::new(u32::from(pcm.channels), pcm.sample_rate, Mode::TRUE_PEAK)?;
    analyzer.add_frames_f32(&pcm.samples)?;
    let mut peak = 0.0_f64;
    for channel in 0..u32::from(pcm.channels) {
        peak = peak.max(analyzer.true_peak(channel)?);
    }
    Ok(peak as f32)
}

fn measure_rms(pcm: &WavPcm, threshold_ratio: f32) -> Result<RmsReport, AnyError> {
    let channels = usize::from(pcm.channels);
    let frames = pcm.samples.len() / channels;
    let window = frames_for_ms(pcm.sample_rate, RMS_WINDOW_MS).max(1);
    let hop = frames_for_ms(pcm.sample_rate, RMS_HOP_MS).max(1);
    if frames < window {
        return Err("preview is shorter than the RMS window".into());
    }
    let mut values = Vec::new();
    let mut start = 0;
    while start + window <= frames {
        values.push(frame_rms(pcm, start, start + window));
        start += hop;
    }
    if values.is_empty() {
        return Err("no RMS windows were measured".into());
    }
    let edge = ((values[0].powi(2) + values[values.len() - 1].powi(2)) * 0.5).sqrt();
    let quietest = values.iter().copied().fold(f32::INFINITY, f32::min);
    let ratio = quietest / edge.max(f32::EPSILON);
    let mut run = 0;
    let mut longest = 0;
    for value in &values {
        if *value / edge.max(f32::EPSILON) < threshold_ratio {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    Ok(RmsReport {
        window_ms: RMS_WINDOW_MS,
        hop_ms: RMS_HOP_MS,
        windows: values.len(),
        quietest_dbfs: amplitude_dbfs(quietest),
        edge_reference_dbfs: amplitude_dbfs(edge),
        quietest_to_edge_ratio: ratio,
        consecutive_below_ratio_windows: longest,
    })
}

fn measure_silence(
    pcm: &WavPcm,
    window_ms: u32,
    threshold_dbfs: f32,
) -> Result<SilenceReport, AnyError> {
    let channels = usize::from(pcm.channels);
    let frames = pcm.samples.len() / channels;
    let window = frames_for_ms(pcm.sample_rate, window_ms).max(1);
    let hop = frames_for_ms(pcm.sample_rate, SILENCE_HOP_MS).max(1);
    if frames < window {
        return Err("preview is shorter than the silence window".into());
    }
    let threshold = 10.0_f32.powf(threshold_dbfs / 20.0);
    let mut detected = 0;
    let mut run = 0;
    let mut longest = 0;
    let mut start = 0;
    while start + window <= frames {
        if frame_rms(pcm, start, start + window) < threshold {
            detected += 1;
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
        start += hop;
    }
    Ok(SilenceReport {
        window_ms,
        threshold_dbfs,
        windows_detected: detected,
        longest_run_ms: if longest == 0 {
            0
        } else {
            window_ms + (longest as u32 - 1) * SILENCE_HOP_MS
        },
    })
}

fn frame_rms(pcm: &WavPcm, first: usize, end: usize) -> f32 {
    let channels = usize::from(pcm.channels);
    let samples = &pcm.samples[first * channels..end * channels];
    (samples
        .iter()
        .map(|sample| f64::from(*sample) * f64::from(*sample))
        .sum::<f64>()
        / samples.len() as f64)
        .sqrt() as f32
}

#[cfg(test)]
fn measure_kick_phase(pcm: &WavPcm, bpm: f32) -> Option<KickPhaseReport> {
    if !bpm.is_finite() || bpm <= 0.0 {
        return None;
    }
    let interval = 60.0 / bpm;
    let frames = pcm.samples.len() / usize::from(pcm.channels);
    let expected_beats = (frames as f32 / pcm.sample_rate as f32 / interval).ceil() as usize;
    measure_kick_phase_with_expected(pcm, bpm, Some(expected_beats))
}

fn measure_kick_phase_with_expected(
    pcm: &WavPcm,
    bpm: f32,
    expected_beats: Option<usize>,
) -> Option<KickPhaseReport> {
    if !bpm.is_finite() || bpm <= 0.0 {
        return None;
    }
    let interval = 60.0 / bpm;
    let channels = usize::from(pcm.channels);
    let frames = pcm.samples.len() / channels;
    let expected_beats = expected_beats.unwrap_or(0);
    let mut issues = Vec::new();
    if expected_beats == 0 {
        issues.push("kick_expected_beats_unavailable".to_owned());
    }
    let mut filter = LowBandFilter::new(pcm.sample_rate)?;
    let block_frames = frames_for_ms(pcm.sample_rate, 5).max(1);
    let mut energies = Vec::with_capacity(frames.div_ceil(block_frames));
    let mut sum = 0.0_f64;
    let mut count = 0;
    for frame in 0..frames {
        let mono = pcm.samples[frame * channels..(frame + 1) * channels]
            .iter()
            .copied()
            .sum::<f32>()
            / channels as f32;
        let low = filter.process(mono);
        sum += f64::from(low) * f64::from(low);
        count += 1;
        if count == block_frames || frame + 1 == frames {
            energies.push((sum / count as f64).sqrt() as f32);
            sum = 0.0;
            count = 0;
        }
    }
    if energies.len() < 3 {
        issues.push("kick_onsets_unavailable".to_owned());
        return Some(unavailable_kick_phase(bpm, expected_beats, 0, 0, issues));
    }
    let mut onsets = Vec::with_capacity(energies.len());
    onsets.push(0.0);
    onsets.extend(energies.windows(2).map(|pair| (pair[1] - pair[0]).max(0.0)));
    let maximum = onsets.iter().copied().fold(0.0_f32, f32::max);
    if maximum <= f32::EPSILON {
        issues.push("kick_onsets_unavailable".to_owned());
        return Some(unavailable_kick_phase(bpm, expected_beats, 0, 0, issues));
    }
    let mut sorted = onsets.clone();
    sorted.sort_by(f32::total_cmp);
    let median = sorted[sorted.len() / 2];
    let mut deviations = sorted
        .iter()
        .map(|value| (*value - median).abs())
        .collect::<Vec<_>>();
    deviations.sort_by(f32::total_cmp);
    let mad = deviations[deviations.len() / 2];
    let threshold = (median + 4.0 * mad).max(maximum * 0.035);
    let block_seconds = block_frames as f32 / pcm.sample_rate as f32;
    let minimum_blocks = (0.060 / block_seconds).ceil() as usize;
    let mut peaks: Vec<(f32, f32)> = Vec::new();
    for index in 1..onsets.len() - 1 {
        let value = onsets[index];
        if value < threshold || value < onsets[index - 1] || value < onsets[index + 1] {
            continue;
        }
        let position = index as f32 * block_seconds;
        if let Some(last) = peaks.last_mut()
            && index.saturating_sub((last.0 / block_seconds).round() as usize) < minimum_blocks
        {
            if value > last.1 {
                *last = (position, value);
            }
        } else {
            peaks.push((position, value));
        }
    }
    if peaks.len() < 3 {
        issues.push("kick_matches_insufficient".to_owned());
        return Some(unavailable_kick_phase(
            bpm,
            expected_beats,
            peaks.len(),
            0,
            issues,
        ));
    }
    let duration = frames as f32 / pcm.sample_rate as f32;
    let expected = (duration / interval).ceil() as usize;
    let maximum_peaks = expected.saturating_mul(3).max(3);
    if peaks.len() > maximum_peaks {
        peaks.sort_by(|left, right| right.1.total_cmp(&left.1));
        peaks.truncate(maximum_peaks);
        peaks.sort_by(|left, right| left.0.total_cmp(&right.0));
    }
    let anchor_end = (interval * 4.0).min(duration * 0.4).max(interval * 2.0);
    let anchor_peaks = peaks
        .iter()
        .copied()
        .filter(|peak| peak.0 <= anchor_end)
        .collect::<Vec<_>>();
    if anchor_peaks.len() < 2 {
        issues.push("kick_phase_anchor_unavailable".to_owned());
        return Some(unavailable_kick_phase(
            bpm,
            expected_beats,
            peaks.len(),
            0,
            issues,
        ));
    }
    let sigma = 0.025_f32;
    let phase = anchor_peaks
        .iter()
        .map(|candidate| candidate.0.rem_euclid(interval))
        .max_by(|left, right| {
            phase_score(*left, &anchor_peaks, interval, sigma).total_cmp(&phase_score(
                *right,
                &anchor_peaks,
                interval,
                sigma,
            ))
        })?;
    let first_grid_index = ((0.0 - phase) / interval).ceil() as i32;
    let last_grid_index = ((duration - phase) / interval).floor() as i32;
    if last_grid_index < first_grid_index {
        issues.push("kick_expected_beats_unavailable".to_owned());
        return Some(unavailable_kick_phase(
            bpm,
            expected_beats,
            peaks.len(),
            0,
            issues,
        ));
    }
    let mut matched = HashMap::<i32, (f32, f32)>::new();
    for peak in &peaks {
        let grid_index = ((peak.0 - phase) / interval).round() as i32;
        if grid_index < first_grid_index || grid_index > last_grid_index {
            continue;
        }
        let expected_position = phase + grid_index as f32 * interval;
        let error = (peak.0 - expected_position).abs();
        if error > interval * 0.45 {
            continue;
        }
        match matched.entry(grid_index) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                if peak.1 > entry.get().1 {
                    entry.insert((error, peak.1));
                }
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert((error, peak.1));
            }
        }
    }
    if matched.len() < 3 {
        issues.push("kick_matches_insufficient".to_owned());
        return Some(unavailable_kick_phase(
            bpm,
            expected_beats,
            matched.len(),
            matched.len(),
            issues,
        ));
    }
    let mut matched = matched.into_iter().collect::<Vec<_>>();
    matched.sort_by_key(|(grid_index, _)| *grid_index);
    let mut errors = matched
        .iter()
        .map(|(_, (error, _))| *error * 1000.0)
        .collect::<Vec<_>>();
    errors.sort_by(f32::total_cmp);
    let p95_index = ((errors.len() as f32 * 0.95).ceil() as usize)
        .saturating_sub(1)
        .min(errors.len() - 1);
    let handoff_ms = matched.last()?.1.0 * 1000.0;
    let matched_beats = matched.len().min(expected_beats);
    let beat_coverage = if expected_beats == 0 {
        0.0
    } else {
        matched_beats as f32 / expected_beats as f32
    };
    Some(KickPhaseReport {
        expected_bpm: bpm,
        candidate_onsets: peaks.len(),
        matched_beats,
        expected_beats,
        beat_coverage,
        p95_ms: errors[p95_index],
        max_ms: errors.last().copied().unwrap_or(0.0),
        handoff_ms,
        issues,
    })
}

fn unavailable_kick_phase(
    bpm: f32,
    expected_beats: usize,
    candidate_onsets: usize,
    matched_beats: usize,
    issues: Vec<String>,
) -> KickPhaseReport {
    KickPhaseReport {
        expected_bpm: bpm,
        candidate_onsets,
        matched_beats,
        expected_beats,
        beat_coverage: 0.0,
        p95_ms: f32::MAX,
        max_ms: f32::MAX,
        handoff_ms: f32::MAX,
        issues,
    }
}

fn phase_score(phase: f32, peaks: &[(f32, f32)], interval: f32, sigma: f32) -> f32 {
    peaks
        .iter()
        .map(|peak| {
            let error = circular_error(peak.0, phase, interval);
            peak.1 * (-0.5 * (error / sigma).powi(2)).exp()
        })
        .sum()
}

fn circular_error(position: f32, phase: f32, interval: f32) -> f32 {
    let difference = (position - phase).rem_euclid(interval);
    difference.min(interval - difference)
}

fn frames_for_ms(sample_rate: u32, milliseconds: u32) -> usize {
    ((u64::from(sample_rate) * u64::from(milliseconds)) / 1000) as usize
}

fn amplitude_dbfs(amplitude: f32) -> f32 {
    if amplitude <= f32::EPSILON {
        -120.0
    } else {
        20.0 * amplitude.log10()
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::{Shutdown, TcpListener, TcpStream},
        sync::{Mutex, OnceLock},
        thread,
        time::{Duration, Instant},
    };

    use wotoha_core::{PreparedSource, TrackMetadata};

    static PROCESS_STATE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    const TEST_THRESHOLDS: Thresholds = Thresholds {
        min_kick_coverage: 0.65,
        min_matched_kicks: 8,
        kick_phase_p95_ms: 25.0,
        kick_phase_max_ms: 35.0,
        kick_handoff_ms: 25.0,
        minimum_rms_gap_ratio: 0.35,
        silence_window_ms: 250,
        silence_threshold_dbfs: -45.0,
        maximum_sample_peak_dbfs: -1.0,
        maximum_true_peak_dbtp: -1.0,
    };

    #[tokio::test]
    async fn stage_timeout_is_finite_and_allows_continuation() {
        let timeout = run_stage_until(
            std::future::pending::<()>(),
            Instant::now() + Duration::from_millis(10),
            "test_timeout",
        )
        .await
        .expect_err("pending stage must time out");
        assert_eq!(timeout.code, "test_timeout");

        let continued = run_stage_until(
            async { 42_u8 },
            Instant::now() + Duration::from_secs(1),
            "continuation_timeout",
        )
        .await
        .expect("following stage should continue");
        assert_eq!(continued, 42);
    }

    #[test]
    fn acquisition_refresh_codes_are_bounded_and_explicit() {
        for code in [
            "http_403",
            "http_410",
            "source_request_failed",
            "range_content_range_mismatch",
            "range_short_nonfinal",
            "hash_timeout",
        ] {
            assert!(retryable_acquisition_code(code), "{code} should refresh");
        }
        for code in [
            "http_404",
            "source_length_exceeded",
            "source_url_not_https",
            "range_full_body_media_type_invalid",
        ] {
            assert!(
                !retryable_acquisition_code(code),
                "{code} must not retry the same acquisition class"
            );
        }
        assert_eq!(MAX_ACQUISITION_CYCLES, 3);
        assert_eq!(FIXTURE_TOTAL_TIMEOUT, Duration::from_secs(900));
        assert_eq!(http_error_code(StatusCode::GONE), "http_410");
    }

    #[test]
    fn provider_expiry_is_not_retried_on_the_same_signed_url() {
        for (status, code) in [(401, "http_401"), (403, "http_403"), (410, "http_410")] {
            let result = run_mock_hash(vec![mock_response(status, None, b"")], Some(10), Some(4));
            assert!(!result.outcome.ok, "unexpected success: {result:?}");
            assert_eq!(result.request_count, 1, "{result:?}");
            assert_eq!(result.outcome.error_code.as_deref(), Some(code));
        }
    }

    fn identity_fixture() -> TrackFixture {
        TrackFixture {
            id: "identity".to_owned(),
            title: "NOTD & Maia Wright - Lover Online (Extended Mix)".to_owned(),
            genre: "house".to_owned(),
            url: "https://youtu.be/tbM2h4rWcmg?list=playlist".to_owned(),
            expected_video_id: "tbM2h4rWcmg".to_owned(),
            expected_title_tokens: vec![
                "NOTD".to_owned(),
                "Maia Wright".to_owned(),
                "Lover Online".to_owned(),
                "Extended Mix".to_owned(),
            ],
        }
    }

    fn identity_request(title: &str, canonical_url: &str, canonical_key: &str) -> TrackRequest {
        TrackRequest::new(
            "youtube",
            canonical_key,
            canonical_url,
            canonical_url,
            canonical_url,
            PreparedSource::http(
                canonical_url,
                Vec::<wotoha_core::PreparedHeader>::new(),
                None,
                None,
            ),
            TrackMetadata::new(title, "placeholder", canonical_url, None, None),
        )
    }

    #[test]
    fn identity_accepts_case_punctuation_and_unicode_separator_variants() {
        let fixture = identity_fixture();
        let request = identity_request(
            "ＮＯＴＤ ＆ Maia Wright – lover online （Extended Mix）",
            "https://www.youtube.com/watch?v=tbM2h4rWcmg&list=other-playlist",
            "youtube:video:tbM2h4rWcmg",
        );
        validate_fixture_identity(&fixture, &request).unwrap();
    }

    #[test]
    fn identity_rejects_wrong_title_and_video_id() {
        let fixture = identity_fixture();
        let wrong_title = identity_request(
            "NOTD & Maia Wright - Other Song (Extended Mix)",
            "https://www.youtube.com/watch?v=tbM2h4rWcmg",
            "youtube:video:tbM2h4rWcmg",
        );
        assert_eq!(
            validate_fixture_identity(&fixture, &wrong_title),
            Err("identity_title_token_mismatch")
        );
        let wrong_video = identity_request(
            "NOTD & Maia Wright - Lover Online (Extended Mix)",
            "https://www.youtube.com/watch?v=wrong-video",
            "youtube:video:wrong-video",
        );
        assert_eq!(
            validate_fixture_identity(&fixture, &wrong_video),
            Err("identity_video_id_mismatch")
        );
    }

    #[test]
    fn classical_backend_cannot_pass_strict_neural_gate() {
        assert!(!AnalysisBackend::ClassicalTransientFailure.is_fresh_neural());
        assert!(!AnalysisBackend::ClassicalPermanentIneligible.is_fresh_neural());
        assert!(!AnalysisBackend::CachedNeural.is_fresh_neural());
        assert!(!AnalysisBackend::CachedClassicalTransientFailure.is_fresh_neural());
        assert!(AnalysisBackend::Neural.is_fresh_neural());
        assert!(!strict_analysis_passes(
            true,
            Some(AnalysisBackend::ClassicalTransientFailure)
        ));
        assert!(strict_analysis_passes(true, Some(AnalysisBackend::Neural)));
    }

    #[test]
    fn backend_and_reason_are_serialized_in_track_report() {
        let fixture = identity_fixture();
        let mut report = TrackReport::failed(&fixture, Vec::new(), "analysis_backend_not_neural");
        report.analysis_backend = Some(
            AnalysisBackend::ClassicalTransientFailure
                .as_str()
                .to_owned(),
        );
        report.analysis_backend_reason =
            Some(analysis_backend_reason(AnalysisBackend::ClassicalTransientFailure).to_owned());
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("ClassicalTransientFailure"), "{json}");
        assert!(
            json.contains("fresh_classical_transient_fallback"),
            "{json}"
        );
        report.analysis_backend = Some(
            AnalysisBackend::CachedClassicalTransientFailure
                .as_str()
                .to_owned(),
        );
        report.analysis_backend_reason = Some(
            analysis_backend_reason(AnalysisBackend::CachedClassicalTransientFailure).to_owned(),
        );
        let cached_json = serde_json::to_string(&report).unwrap();
        assert!(
            cached_json.contains("CachedClassicalTransientFailure"),
            "{cached_json}"
        );
        assert!(
            cached_json.contains("classical_transient_fallback_cache_hit"),
            "{cached_json}"
        );
    }

    #[test]
    fn quality_report_serializes_beat_phase_coverage_for_raw_and_rejected_plans() {
        let quality = AutoMixQualityReport {
            issues: Vec::new(),
            overlap: Duration::from_secs(16),
            beat_pairs_checked: 8,
            beat_phase_coverage: Some(0.75),
            max_beat_phase_error: Some(Duration::from_millis(4)),
            handoff_beat_phase_error: Some(Duration::from_millis(5)),
            downbeat_pairs_checked: 2,
            max_downbeat_phase_error: None,
            handoff_downbeat_phase_error: None,
            phrase_pairs_checked: 1,
            max_phrase_phase_error: None,
            handoff_phrase_phase_error: None,
            phrase_boundary_bars: None,
            structure_overlap_ratio: None,
            harmonic_compatibility: None,
            low_handoff_min: None,
            low_handoff_max: None,
            vocal_overlap_samples_checked: 0,
            max_dual_vocal_risk: None,
            energy_samples_checked: 0,
            min_mix_energy_ratio: None,
            max_mix_energy_ratio: None,
            max_mix_energy_step: None,
            handoff_mix_energy_ratio: None,
            handoff_incoming_mix_share: None,
            max_tempo_speed_step: None,
        };
        let json = serde_json::to_value(QualityReport::from(&quality)).unwrap();
        assert_eq!(json["beat_phase_coverage"], serde_json::json!(0.75));
        assert_eq!(json["beat_pairs"], serde_json::json!(8));
    }

    #[test]
    fn aligned_generated_kicks_have_tight_phase() {
        let pcm = generated_kicks(48_000, 8.0, 120.0, &[0.1]);
        let report = measure_kick_phase(&pcm, 120.0).expect("kick phase should be measurable");
        assert!(report.p95_ms <= 10.0, "{report:?}");
        assert!(report.max_ms <= 15.0, "{report:?}");
        assert!(report.handoff_ms <= 10.0, "{report:?}");
    }

    #[test]
    fn generated_phase_change_exposes_kick_offset() {
        let mut pcm = generated_kicks(48_000, 8.0, 120.0, &[0.1]);
        let shifted = generated_kicks(48_000, 8.0, 120.0, &[0.18]);
        let midpoint = pcm.samples.len() / 2;
        pcm.samples[midpoint..].copy_from_slice(&shifted.samples[midpoint..]);
        let report = measure_kick_phase(&pcm, 120.0).expect("kick phase should be measurable");
        assert!(report.p95_ms >= 60.0, "{report:?}");
        assert!(report.max_ms >= 60.0, "{report:?}");
        assert!(report.handoff_ms >= 60.0, "{report:?}");
    }

    #[test]
    fn generated_long_overlap_with_only_three_kicks_fails_count_and_coverage() {
        let mut pcm = generated_kicks(48_000, 12.0, 120.0, &[0.1]);
        let first_unverified_frame = (1.6 * pcm.sample_rate as f32) as usize;
        pcm.samples[first_unverified_frame..].fill(0.0);
        let kick = measure_kick_phase(&pcm, 120.0).expect("kick phase should be measurable");
        assert!(kick.matched_beats <= 3, "{kick:?}");
        let mut pcm_report = pcm_report_with_kick(kick);
        let mut failures = Vec::new();
        apply_pcm_thresholds(&mut pcm_report, &TEST_THRESHOLDS, &mut failures);
        assert!(
            failures.contains(&"kick_matched_beats".to_owned()),
            "{failures:?}"
        );
        assert!(
            failures.contains(&"kick_coverage".to_owned()),
            "{failures:?}"
        );
    }

    #[test]
    fn generated_overlap_with_eight_good_kicks_passes_count_and_coverage() {
        let pcm = generated_kicks(48_000, 8.0, 120.0, &[0.1]);
        let kick = measure_kick_phase(&pcm, 120.0).expect("kick phase should be measurable");
        assert!(kick.matched_beats >= 8, "{kick:?}");
        assert!(kick.beat_coverage >= 0.65, "{kick:?}");
        let mut pcm_report = pcm_report_with_kick(kick);
        let mut failures = Vec::new();
        apply_pcm_thresholds(&mut pcm_report, &TEST_THRESHOLDS, &mut failures);
        assert!(
            !failures.contains(&"kick_matched_beats".to_owned()),
            "{failures:?}"
        );
        assert!(
            !failures.contains(&"kick_coverage".to_owned()),
            "{failures:?}"
        );
    }

    #[test]
    fn missing_overlap_grid_fails_closed_instead_of_dividing_by_zero() {
        let pcm = generated_kicks(48_000, 8.0, 120.0, &[0.1]);
        let wav = encode_pcm16_wav(pcm.sample_rate, pcm.channels, &pcm.samples);
        let report = measure_wav_with_overlap(&wav, Some(120.0), None, &TEST_THRESHOLDS).unwrap();
        let kick = report
            .kick_phase
            .as_ref()
            .expect("valid BPM should produce a report");
        assert_eq!(kick.expected_beats, 0);
        assert_eq!(kick.beat_coverage, 0.0);
        assert!(
            kick.issues
                .contains(&"kick_expected_beats_unavailable".to_owned())
        );
        let mut failures = Vec::new();
        let mut report = report;
        apply_pcm_thresholds(&mut report, &TEST_THRESHOLDS, &mut failures);
        assert!(
            failures.contains(&"kick_expected_beats_unavailable".to_owned()),
            "{failures:?}"
        );
        assert!(
            failures.contains(&"kick_coverage".to_owned()),
            "{failures:?}"
        );
    }

    #[test]
    fn pcm_remeasurement_detects_a_quiet_gap() {
        let sample_rate = 48_000;
        let mut samples = vec![0.12_f32; sample_rate as usize * 3 * 2];
        for frame in sample_rate as usize..sample_rate as usize + 36_000 {
            samples[frame * 2] = 0.0;
            samples[frame * 2 + 1] = 0.0;
        }
        let wav = encode_pcm16_wav(sample_rate, 2, &samples);
        let report = measure_wav(&wav, None, &TEST_THRESHOLDS).unwrap();
        assert!(report.rms_500ms.quietest_to_edge_ratio < 0.35);
        assert!(report.silence.windows_detected > 0);
        assert!(report.silence.longest_run_ms >= 250);
    }

    #[test]
    fn manifest_requires_all_self_pairs_and_two_cross_pairs() {
        let tracks = (0..4)
            .map(|index| TrackFixture {
                id: format!("track-{index}"),
                title: format!("Track {index} Extended Mix"),
                genre: "house".to_owned(),
                url: format!("https://www.youtube.com/watch?v=test{index}"),
                expected_video_id: format!("test{index}"),
                expected_title_tokens: vec![format!("Track {index}"), "Extended Mix".to_owned()],
            })
            .collect::<Vec<_>>();
        let mut pairs = tracks
            .iter()
            .map(|track| PairFixture {
                outgoing: track.id.clone(),
                incoming: track.id.clone(),
                expected_transition_kind: "BeatMatched".to_owned(),
            })
            .collect::<Vec<_>>();
        pairs.push(PairFixture {
            outgoing: "track-0".to_owned(),
            incoming: "track-1".to_owned(),
            expected_transition_kind: "BeatMatched".to_owned(),
        });
        pairs.push(PairFixture {
            outgoing: "track-1".to_owned(),
            incoming: "track-2".to_owned(),
            expected_transition_kind: "BeatMatched".to_owned(),
        });
        let manifest = CorpusManifest {
            schema_version: 1,
            tracks,
            pairs,
            thresholds: TEST_THRESHOLDS,
        };
        validate_manifest(&manifest).unwrap();
    }

    #[test]
    fn cache_path_is_always_a_private_run_child() {
        let run_dir = env::temp_dir().join("wotoha-automix-corpus-test-cache");
        assert_eq!(
            analysis_cache_dir(&run_dir),
            run_dir.join(".wotoha-analysis")
        );
    }

    #[test]
    fn inherited_cache_sentinel_is_not_used_or_modified() {
        let _lock = PROCESS_STATE_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap();
        let original_dir = env::current_dir().unwrap();
        let sentinel =
            env::temp_dir().join(format!("wotoha-automix-corpus-sentinel-{}", process::id()));
        fs::create_dir_all(&sentinel).unwrap();
        let marker = sentinel.join("marker");
        fs::write(&marker, b"sentinel").unwrap();
        let original_cache = env::var_os(ANALYSIS_CACHE_ENV);
        unsafe { env::set_var(ANALYSIS_CACHE_ENV, &sentinel) };
        let run_dir = create_run_dir().unwrap();
        {
            let _environment = RunEnvironment::new(&original_dir, &run_dir, false).unwrap();
            assert_eq!(
                env::var_os(ANALYSIS_CACHE_ENV),
                Some(analysis_cache_dir(&run_dir).into())
            );
            assert_eq!(fs::read(&marker).unwrap(), b"sentinel");
        }
        assert_eq!(
            env::var_os(ANALYSIS_CACHE_ENV),
            Some(sentinel.clone().into())
        );
        assert_eq!(fs::read(&marker).unwrap(), b"sentinel");
        unsafe {
            match original_cache {
                Some(value) => env::set_var(ANALYSIS_CACHE_ENV, value),
                None => env::remove_var(ANALYSIS_CACHE_ENV),
            }
        }
        let _ = fs::remove_dir_all(sentinel);
    }

    #[test]
    fn cleanup_removes_only_the_unique_run_directory() {
        let run_dir = create_run_dir().unwrap();
        fs::write(run_dir.join("marker"), b"temporary").unwrap();
        remove_run_dir(&run_dir).unwrap();
        assert!(!run_dir.exists());
    }

    #[test]
    fn keep_artifacts_opt_in_preserves_the_run_directory() {
        let _lock = PROCESS_STATE_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap();
        let original_dir = env::current_dir().unwrap();
        let run_dir = create_run_dir().unwrap();
        fs::write(run_dir.join("marker"), b"kept").unwrap();
        {
            let _environment = RunEnvironment::new(&original_dir, &run_dir, true).unwrap();
        }
        assert_eq!(fs::read(run_dir.join("marker")).unwrap(), b"kept");
        remove_run_dir(&run_dir).unwrap();
    }

    #[test]
    fn ranged_hash_accepts_verified_multi_range_body() {
        let responses = vec![
            mock_response(206, Some("bytes 0-3/10"), b"abcd"),
            mock_response(206, Some("bytes 4-7/10"), b"efgh"),
            mock_response(206, Some("bytes 8-9/10"), b"ij"),
        ];
        let result = run_mock_hash(responses, Some(10), Some(4));
        assert!(result.outcome.ok, "{result:?}");
        assert_eq!(result.request_count, 3, "{result:?}");
        assert_eq!(
            result.outcome.digest.unwrap().sha256,
            sha256_hex(b"abcdefghij")
        );
    }

    #[test]
    fn ranged_hash_rejects_wrong_start_repeated_range_missing_header_short_nonfinal_and_total() {
        let cases = [
            (
                vec![
                    mock_response(206, Some("bytes 1-4/10"), b"bcde"),
                    mock_response(206, Some("bytes 1-4/10"), b"bcde"),
                ],
                2,
                "range_content_range_mismatch",
            ),
            (
                vec![
                    mock_response(206, Some("bytes 0-3/10"), b"abcd"),
                    mock_response(206, Some("bytes 0-3/10"), b"abcd"),
                    mock_response(206, Some("bytes 0-3/10"), b"abcd"),
                ],
                3,
                "range_content_range_mismatch",
            ),
            (
                vec![
                    mock_response(206, None, b"abcd"),
                    mock_response(206, None, b"abcd"),
                ],
                2,
                "range_missing_content_range",
            ),
            (
                vec![
                    mock_response(206, Some("bytes 0-2/10"), b"abc"),
                    mock_response(206, Some("bytes 0-2/10"), b"abc"),
                ],
                2,
                "range_short_nonfinal",
            ),
            (
                vec![
                    mock_response(206, Some("bytes 0-3/11"), b"abcd"),
                    mock_response(206, Some("bytes 0-3/11"), b"abcd"),
                ],
                2,
                "range_content_range_mismatch",
            ),
            (
                vec![
                    mock_response(206, Some("bytes 0-3/10"), b"abcd"),
                    mock_response(200, None, b"abcdefghij"),
                    mock_response(200, None, b"abcdefghij"),
                ],
                3,
                "range_full_body_media_type_invalid",
            ),
        ];
        for (responses, expected_requests, expected_error) in cases {
            let result = run_mock_hash(responses, Some(10), Some(4));
            assert!(!result.outcome.ok, "unexpected success: {result:?}");
            assert_eq!(result.request_count, expected_requests, "{result:?}");
            assert_eq!(
                result.outcome.error_code.as_deref(),
                Some(expected_error),
                "{result:?}"
            );
        }
    }

    #[test]
    fn full_200_is_accepted_only_when_body_matches_known_length() {
        let result = run_mock_hash(
            vec![mock_full_response(
                200,
                "audio/webm",
                Some(10),
                b"abcdefghij",
            )],
            Some(10),
            None,
        );
        assert!(result.outcome.ok, "{result:?}");
        assert_eq!(result.request_count, 1, "{result:?}");
    }

    #[test]
    fn ranged_full_200_is_accepted_as_one_verified_media_body() {
        let result = run_mock_hash(
            vec![mock_full_response(
                200,
                "application/octet-stream",
                Some(10),
                b"abcdefghij",
            )],
            None,
            Some(4),
        );
        assert!(result.outcome.ok, "{result:?}");
        assert_eq!(result.request_count, 1, "{result:?}");
        assert_eq!(
            result.outcome.digest.unwrap().sha256,
            sha256_hex(b"abcdefghij")
        );
    }

    #[test]
    fn full_200_rejects_missing_or_wrong_media_type_and_bad_lengths() {
        let cases = [
            (
                repeated_mock_response(mock_full_response_without_length(
                    200,
                    "audio/webm",
                    b"abcdefghij",
                )),
                None,
                Some(4),
                "range_full_body_length_missing",
            ),
            (
                repeated_mock_response(mock_full_response(
                    200,
                    "text/html",
                    Some(10),
                    b"abcdefghij",
                )),
                None,
                Some(4),
                "range_full_body_media_type_invalid",
            ),
            (
                repeated_mock_response(mock_full_response(
                    200,
                    "audio/webm",
                    Some(10),
                    b"abcdefghi",
                )),
                None,
                Some(4),
                "source_body_failed",
            ),
            (
                repeated_mock_response(mock_full_response(
                    200,
                    "audio/webm",
                    Some(9),
                    b"abcdefghi",
                )),
                Some(10),
                Some(4),
                "range_full_body_length_mismatch",
            ),
            (
                repeated_mock_response(mock_full_response(
                    200,
                    "audio/webm",
                    Some(11),
                    b"abcdefghijk",
                )),
                Some(10),
                Some(4),
                "range_full_body_length_mismatch",
            ),
        ];
        for (responses, content_length, chunk_size, expected_error) in cases {
            let result = run_mock_hash(responses, content_length, chunk_size);
            assert!(!result.outcome.ok, "unexpected success: {result:?}");
            assert_eq!(result.request_count, 2, "{result:?}");
            assert_eq!(result.outcome.error_code.as_deref(), Some(expected_error));
        }
    }

    #[derive(Clone, Debug)]
    struct MockResponse {
        status: u16,
        content_range: Option<String>,
        content_type: Option<String>,
        content_length: Option<u64>,
        include_content_length: bool,
        body: Vec<u8>,
    }

    #[derive(Debug)]
    struct MockHashResult {
        outcome: AcquisitionOutcome,
        request_count: usize,
    }

    fn mock_response(status: u16, content_range: Option<&str>, body: &[u8]) -> MockResponse {
        MockResponse {
            status,
            content_range: content_range.map(str::to_owned),
            content_type: None,
            content_length: None,
            include_content_length: true,
            body: body.to_vec(),
        }
    }

    fn mock_full_response(
        status: u16,
        content_type: &str,
        content_length: Option<u64>,
        body: &[u8],
    ) -> MockResponse {
        MockResponse {
            status,
            content_range: None,
            content_type: Some(content_type.to_owned()),
            content_length,
            include_content_length: true,
            body: body.to_vec(),
        }
    }

    fn mock_full_response_without_length(
        status: u16,
        content_type: &str,
        body: &[u8],
    ) -> MockResponse {
        MockResponse {
            status,
            content_range: None,
            content_type: Some(content_type.to_owned()),
            content_length: None,
            include_content_length: false,
            body: body.to_vec(),
        }
    }

    fn repeated_mock_response(response: MockResponse) -> Vec<MockResponse> {
        vec![response; HASH_REQUEST_RETRIES]
    }

    fn run_mock_hash(
        responses: Vec<MockResponse>,
        content_length: Option<u64>,
        chunk_size: Option<u64>,
    ) -> MockHashResult {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || serve_mock_responses(listener, responses));
        let request = TrackRequest::new(
            "test",
            "hash",
            "http://example.test/source",
            "http://example.test/source",
            "http://example.test/source",
            PreparedSource::http_with_range(
                format!("http://{address}/source"),
                Vec::<wotoha_core::PreparedHeader>::new(),
                content_length,
                chunk_size,
                None,
            ),
            TrackMetadata::new("test", "test", "http://example.test/source", None, None),
        );
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let outcome =
            runtime.block_on(async { hash_prepared_source(&mock_http_client(), &request).await });
        let server_result = server
            .join()
            .expect("mock HTTP server thread panicked")
            .expect("mock HTTP server timed out or failed");
        MockHashResult {
            outcome,
            request_count: server_result,
        }
    }

    fn mock_http_client() -> Client {
        Client::builder()
            .connect_timeout(Duration::from_millis(250))
            .timeout(Duration::from_secs(1))
            .build()
            .unwrap()
    }

    fn serve_mock_responses(
        listener: TcpListener,
        responses: Vec<MockResponse>,
    ) -> Result<usize, String> {
        listener
            .set_nonblocking(true)
            .map_err(|error| format!("set listener nonblocking: {error}"))?;
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut request_count = 0;
        for response in responses {
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            return Err(format!(
                                "timed out waiting for request {}",
                                request_count + 1
                            ));
                        }
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => return Err(format!("accept request: {error}")),
                }
            };
            stream
                .set_nonblocking(false)
                .map_err(|error| format!("set stream blocking: {error}"))?;
            stream
                .set_read_timeout(Some(Duration::from_millis(300)))
                .map_err(|error| format!("set read timeout: {error}"))?;
            stream
                .set_write_timeout(Some(Duration::from_millis(300)))
                .map_err(|error| format!("set write timeout: {error}"))?;
            read_mock_request_headers(&mut stream)?;
            let range = response
                .content_range
                .as_deref()
                .map(|value| format!("Content-Range: {value}\r\n"))
                .unwrap_or_default();
            let content_type = response
                .content_type
                .as_deref()
                .map(|value| format!("Content-Type: {value}\r\n"))
                .unwrap_or_default();
            let content_length = response
                .content_length
                .unwrap_or(response.body.len() as u64);
            let content_length = response
                .include_content_length
                .then(|| format!("Content-Length: {content_length}\r\n"))
                .unwrap_or_default();
            let headers = format!(
                "HTTP/1.1 {} Test\r\n{}{}{}Connection: close\r\n\r\n",
                response.status, content_length, content_type, range
            );
            stream
                .write_all(headers.as_bytes())
                .map_err(|error| format!("write response headers: {error}"))?;
            stream
                .write_all(&response.body)
                .map_err(|error| format!("write response body: {error}"))?;
            let _ = stream.shutdown(Shutdown::Both);
            request_count += 1;
        }
        Ok(request_count)
    }

    fn read_mock_request_headers(stream: &mut TcpStream) -> Result<(), String> {
        const MAX_REQUEST_HEADERS: usize = 16 * 1024;
        let mut request = Vec::with_capacity(1024);
        let mut buffer = [0_u8; 1024];
        loop {
            let read = stream
                .read(&mut buffer)
                .map_err(|error| format!("read request headers: {error}"))?;
            if read == 0 {
                return Err("client closed before HTTP headers completed".to_owned());
            }
            request.extend_from_slice(&buffer[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                return Ok(());
            }
            if request.len() > MAX_REQUEST_HEADERS {
                return Err("mock request headers exceeded limit".to_owned());
            }
        }
    }

    fn generated_kicks(sample_rate: u32, seconds: f32, bpm: f32, phases: &[f32]) -> WavPcm {
        let frames = (sample_rate as f32 * seconds) as usize;
        let mut samples = vec![0.0_f32; frames];
        let interval = 60.0 / bpm;
        for phase in phases {
            let mut position = *phase;
            while position < seconds {
                let start = (position * sample_rate as f32) as usize;
                let length = (sample_rate as f32 * 0.09) as usize;
                for offset in 0..length.min(frames.saturating_sub(start)) {
                    let time = offset as f32 / sample_rate as f32;
                    samples[start + offset] +=
                        0.35 * (-time * 35.0).exp() * (std::f32::consts::TAU * 85.0 * time).sin();
                }
                position += interval;
            }
        }
        WavPcm {
            sample_rate,
            channels: 1,
            samples,
        }
    }

    fn pcm_report_with_kick(kick_phase: KickPhaseReport) -> PcmReport {
        PcmReport {
            wav_sha256: String::new(),
            sample_rate: 48_000,
            channels: 1,
            duration_ms: 8_000.0,
            sample_peak_dbfs: -10.0,
            true_peak_dbtp: -10.0,
            kick_phase: Some(kick_phase),
            rms_500ms: RmsReport {
                window_ms: 500,
                hop_ms: 250,
                windows: 1,
                quietest_dbfs: -10.0,
                edge_reference_dbfs: -10.0,
                quietest_to_edge_ratio: 1.0,
                consecutive_below_ratio_windows: 0,
            },
            silence: SilenceReport {
                window_ms: 250,
                threshold_dbfs: -45.0,
                windows_detected: 0,
                longest_run_ms: 0,
            },
        }
    }

    fn encode_pcm16_wav(sample_rate: u32, channels: u16, samples: &[f32]) -> Vec<u8> {
        let data_bytes = samples.len() as u32 * 2;
        let mut wav = Vec::with_capacity(data_bytes as usize + 44);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_bytes).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16_u32.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&channels.to_le_bytes());
        wav.extend_from_slice(&sample_rate.to_le_bytes());
        wav.extend_from_slice(&(sample_rate * u32::from(channels) * 2).to_le_bytes());
        wav.extend_from_slice(&(channels * 2).to_le_bytes());
        wav.extend_from_slice(&16_u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_bytes.to_le_bytes());
        for sample in samples {
            let value = (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16;
            wav.extend_from_slice(&value.to_le_bytes());
        }
        wav
    }
}
