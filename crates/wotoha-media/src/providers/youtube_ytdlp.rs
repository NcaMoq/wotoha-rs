use std::{
    collections::HashMap,
    env,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{Mutex, OnceCell, OwnedMutexGuard, Semaphore},
    time::timeout,
};
use wotoha_contracts::PlaybackFailureKind;
use wotoha_core::{
    PreparedHeader, PreparedRangeMode, PreparedSource, TrackMetadata, TrackRequest,
    operational_metrics::{
        MAX_YTDLP_HTTP_REQUEST_DETAILS, YtDlpHttpRequestDetail, YtDlpHttpRequestKind,
        YtDlpWorkerProfile,
    },
};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(25);
const QUEUE_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_CONCURRENCY: usize = 2;
const MAX_STDOUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_STDERR_BYTES: usize = 64 * 1024;
const MAX_HEADERS: usize = 8;
const MAX_HEADER_VALUE_BYTES: usize = 1024;
const YOUTUBE_RANGE_CHUNK_SIZE: u64 = 11_862_014;
const FAST_YOUTUBE_EXTRACTOR_ARGS: &str = "youtube:skip=hls,dash";
const PYTHON_WORKER_SCRIPT: &str = include_str!("yt_dlp_worker.py");
const MAX_WORKER_LINE_BYTES: usize = 4 * 1024 * 1024;

static YTDLP_SLOTS: OnceLock<Semaphore> = OnceLock::new();
static PYTHON_WORKERS: OnceCell<Arc<PythonWorkerPool>> = OnceCell::const_new();

#[derive(Debug, thiserror::Error)]
pub enum YtDlpError {
    #[error("yt-dlp executable is not configured")]
    NotConfigured,
    #[error("yt-dlp path must be absolute: {0}")]
    RelativePath(String),
    #[error("managed yt-dlp override at {path} failed SHA-256 verification")]
    UnverifiedManagedTool { path: String },
    #[error("yt-dlp is unavailable at {path}: {source}")]
    Unavailable {
        path: String,
        source: std::io::Error,
    },
    #[error("yt-dlp queue timed out after {0:?}")]
    Busy(Duration),
    #[error("yt-dlp timed out after {0:?}")]
    Timeout(Duration),
    #[error("yt-dlp failed with status {status}: {message}")]
    Failed { status: String, message: String },
    #[error("yt-dlp returned more than {limit} bytes on {stream}")]
    OutputTooLarge { stream: &'static str, limit: usize },
    #[error("yt-dlp returned invalid JSON: {0}")]
    InvalidJson(serde_json::Error),
    #[error("yt-dlp did not return a playable URL")]
    MissingUrl,
    #[error("yt-dlp returned an unsafe HTTP header")]
    UnsafeHeaders,
    #[error("invalid yt-dlp cookies file: {0}")]
    CookiesFile(String),
    #[error(
        "yt-dlp Python worker requires a configured Python executable and expected API version"
    )]
    PythonWorkerConfiguration,
    #[error("yt-dlp Python worker could not start or communicate safely")]
    PythonWorkerUnavailable,
    #[error("yt-dlp Python worker returned an invalid response")]
    PythonWorkerProtocol,
}

impl YtDlpError {
    pub fn playback_failure_kind(&self) -> Option<PlaybackFailureKind> {
        let YtDlpError::Failed { message, .. } = self else {
            return None;
        };
        Some(classify_failure_message(message))
    }
}

#[derive(Debug, Deserialize)]
struct YtDlpInfo {
    id: String,
    #[serde(default)]
    title: String,
    uploader: Option<String>,
    channel: Option<String>,
    webpage_url: Option<String>,
    thumbnail: Option<String>,
    duration: Option<f64>,
    #[serde(default)]
    is_live: bool,
    url: Option<String>,
    protocol: Option<String>,
    http_headers: Option<HashMap<String, String>>,
    filesize: Option<u64>,
    filesize_approx: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct PythonWorkerResponse {
    id: u64,
    ok: bool,
    info: Option<YtDlpInfo>,
    error: Option<String>,
    api_extract_ms: Option<f64>,
    startup_ms: Option<f64>,
    import_ms: Option<f64>,
    version: Option<String>,
    request_handler: Option<String>,
    profile: Option<PythonWorkerProfileResponse>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PythonWorkerProfileResponse {
    http_requests: u64,
    http_open_us: u64,
    http_body_read_us: u64,
    dns_calls: u64,
    dns_us: u64,
    tcp_connect_calls: u64,
    tcp_connect_us: u64,
    tls_handshakes: u64,
    tls_us: u64,
    js_solver_calls: u64,
    js_solver_us: u64,
    js_runtime_calls: u64,
    js_runtime_us: u64,
    cpu_user_us: u64,
    cpu_system_us: u64,
    child_cpu_user_us: u64,
    child_cpu_system_us: u64,
    rss_highwater_kb: u64,
    child_rss_highwater_kb: u64,
    youtube_requests: u64,
    media_cdn_requests: u64,
    other_requests: u64,
    http_request_details: Vec<PythonHttpRequestDetail>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PythonHttpRequestDetail {
    kind: String,
    status_code: u16,
    open_us: u64,
    body_read_us: u64,
}

struct PythonWorkerPool {
    workers: Vec<Arc<Mutex<Option<PythonWorker>>>>,
    next_worker: AtomicUsize,
}

struct PythonWorker {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    request_id: u64,
}

pub async fn probe(raw_url: &str) -> Result<TrackRequest, YtDlpError> {
    track_request_from_info(raw_url, extract_with_format_fallback(raw_url).await?)
}

pub async fn refresh(request: &TrackRequest) -> Result<TrackRequest, YtDlpError> {
    let mut refreshed = track_request_from_info(
        request.requested_url.as_ref(),
        extract_with_format_fallback(request.canonical_url.as_ref()).await?,
    )?;
    refreshed.metadata = request.metadata.clone();
    Ok(refreshed)
}

async fn extract_with_format_fallback(raw_url: &str) -> Result<YtDlpInfo, YtDlpError> {
    match extract(raw_url, Some(FAST_YOUTUBE_EXTRACTOR_ARGS)).await {
        Ok(info) if info.url.as_deref().is_some_and(|url| !url.is_empty()) => Ok(info),
        Ok(_) => {
            wotoha_core::operational_metrics::operational_metrics().record_yt_dlp_format_fallback();
            tracing::debug!(
                reason = "missing_playable_format",
                "YouTube fast extraction returned no playable format; retrying once with full format discovery"
            );
            extract(raw_url, None).await
        }
        Err(error) if should_retry_with_full_format_discovery(&error) => {
            wotoha_core::operational_metrics::operational_metrics().record_yt_dlp_format_fallback();
            tracing::debug!(
                reason = "requested_format_unavailable",
                "YouTube fast extraction found no requested format; retrying once with full format discovery"
            );
            extract(raw_url, None).await
        }
        Err(error) => Err(error),
    }
}

async fn extract(raw_url: &str, extractor_args: Option<&str>) -> Result<YtDlpInfo, YtDlpError> {
    let slots = YTDLP_SLOTS.get_or_init(|| Semaphore::new(concurrency()));
    let _permit = timeout(QUEUE_TIMEOUT, slots.acquire())
        .await
        .map_err(|_| YtDlpError::Busy(QUEUE_TIMEOUT))?
        .expect("yt-dlp semaphore stays open");
    if python_worker_requested()? {
        return match extract_with_python_worker(raw_url, extractor_args.is_some()).await {
            Ok(info) => Ok(info),
            Err(error) if should_fallback_to_cli(&error) => {
                tracing::warn!(
                    reason = "python_worker_unavailable",
                    "yt-dlp Python worker unavailable; retrying once through the CLI backend"
                );
                wotoha_core::operational_metrics::operational_metrics()
                    .record_yt_dlp_worker_cli_fallback();
                extract_with_cli(raw_url, extractor_args).await
            }
            Err(error) => Err(error),
        };
    }
    extract_with_cli(raw_url, extractor_args).await
}

async fn extract_with_cli(
    raw_url: &str,
    extractor_args: Option<&str>,
) -> Result<YtDlpInfo, YtDlpError> {
    let executable = yt_dlp_path()?;
    let stdout = run_command(
        &executable,
        arguments(raw_url, cookies_file()?, extractor_args),
        extraction_timeout(),
    )
    .await?;
    serde_json::from_slice(&stdout).map_err(YtDlpError::InvalidJson)
}

fn should_fallback_to_cli(error: &YtDlpError) -> bool {
    matches!(
        error,
        YtDlpError::PythonWorkerConfiguration
            | YtDlpError::PythonWorkerUnavailable
            | YtDlpError::PythonWorkerProtocol
    )
}

fn python_worker_requested() -> Result<bool, YtDlpError> {
    parse_python_worker_backend(env::var("WOTOHA_YTDLP_BACKEND").ok().as_deref())
}

fn parse_python_worker_backend(backend: Option<&str>) -> Result<bool, YtDlpError> {
    match backend {
        None | Some("cli") => Ok(false),
        Some("python-worker") => Ok(true),
        Some(_) => Err(YtDlpError::PythonWorkerConfiguration),
    }
}

async fn extract_with_python_worker(raw_url: &str, fast: bool) -> Result<YtDlpInfo, YtDlpError> {
    let pool = PYTHON_WORKERS
        .get_or_try_init(|| async { PythonWorkerPool::start(concurrency()).await.map(Arc::new) })
        .await?;
    pool.extract(raw_url, fast).await
}

impl PythonWorkerPool {
    async fn start(worker_count: usize) -> Result<Self, YtDlpError> {
        let workers =
            futures::future::try_join_all((0..worker_count).map(|_| PythonWorker::start()))
                .await?
                .into_iter()
                .map(|worker| Arc::new(Mutex::new(Some(worker))))
                .collect();
        Ok(Self {
            workers,
            next_worker: AtomicUsize::new(0),
        })
    }

    async fn extract(&self, raw_url: &str, fast: bool) -> Result<YtDlpInfo, YtDlpError> {
        let mut worker = self.lock_available_worker().await;
        if worker.is_none() {
            *worker = Some(PythonWorker::start().await?);
        }
        let result = worker
            .as_mut()
            .expect("worker was started")
            .extract(raw_url, fast)
            .await;
        if matches!(
            result,
            Err(YtDlpError::PythonWorkerUnavailable)
                | Err(YtDlpError::PythonWorkerProtocol)
                | Err(YtDlpError::Timeout(_))
        ) {
            *worker = None;
        }
        result
    }

    async fn lock_available_worker(&self) -> OwnedMutexGuard<Option<PythonWorker>> {
        loop {
            let start = self.next_worker.fetch_add(1, Ordering::Relaxed);
            for offset in 0..self.workers.len() {
                let index = (start + offset) % self.workers.len();
                if let Ok(guard) = Arc::clone(&self.workers[index]).try_lock_owned() {
                    return guard;
                }
            }
            tokio::task::yield_now().await;
        }
    }
}

impl PythonWorker {
    async fn start() -> Result<Self, YtDlpError> {
        let python = python_path()?;
        let expected_version = env::var("WOTOHA_YTDLP_PYTHON_EXPECTED_VERSION")
            .map_err(|_| YtDlpError::PythonWorkerConfiguration)?;
        let cookies = cookies_file()?;
        let deno = deno_path();
        let mut child = Command::new(python)
            .arg("-I")
            .arg("-B")
            .arg("-u")
            .arg("-c")
            .arg(PYTHON_WORKER_SCRIPT)
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|_| YtDlpError::PythonWorkerUnavailable)?;
        let stdin = child
            .stdin
            .take()
            .ok_or(YtDlpError::PythonWorkerUnavailable)?;
        let stdout = child
            .stdout
            .take()
            .ok_or(YtDlpError::PythonWorkerUnavailable)?;
        if let Some(mut stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut discard = [0; 8192];
                while stderr.read(&mut discard).await.unwrap_or(0) > 0 {}
            });
        }
        let mut worker = Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            request_id: 0,
        };
        let init = serde_json::json!({
            "op": "init",
            "id": 0,
            "cookies_file": cookies.map(|path| path.to_string_lossy().into_owned()),
            "deno_path": deno.map(|path| path.to_string_lossy().into_owned()),
        });
        let init_bytes = serde_json::to_vec(&init).map_err(|_| YtDlpError::PythonWorkerProtocol)?;
        let startup_started = Instant::now();
        let response = timeout(extraction_timeout(), async {
            worker.stdin.write_all(&init_bytes).await.map_err(|_| ())?;
            worker.stdin.write_all(b"\n").await.map_err(|_| ())?;
            worker.stdin.flush().await.map_err(|_| ())?;
            read_worker_response(&mut worker.stdout).await
        })
        .await
        .map_err(|_| YtDlpError::Timeout(extraction_timeout()))?
        .map_err(|_| YtDlpError::PythonWorkerUnavailable)?;
        let response: PythonWorkerResponse =
            serde_json::from_slice(&response).map_err(|_| YtDlpError::PythonWorkerProtocol)?;
        if response.id != 0
            || !response.ok
            || response.version.as_deref() != Some(&expected_version)
        {
            let _ = worker.child.kill().await;
            let _ = worker.child.wait().await;
            return Err(YtDlpError::PythonWorkerConfiguration);
        }
        let startup_elapsed = startup_started.elapsed();
        let metrics = wotoha_core::operational_metrics::operational_metrics();
        metrics.observe_yt_dlp_worker_startup(startup_elapsed);
        if let Some(import_ms) = response
            .import_ms
            .filter(|value| value.is_finite() && *value >= 0.0)
        {
            metrics.observe_yt_dlp_worker_import(Duration::from_secs_f64(import_ms / 1000.0));
        }
        if let Some(handler) = response.request_handler.as_deref() {
            metrics.record_yt_dlp_worker_handler(handler);
            tracing::debug!(
                request_handler = handler,
                "yt-dlp worker selected HTTP request handler"
            );
        }
        if let Some(reported_ms) = response
            .startup_ms
            .filter(|value| value.is_finite() && *value >= 0.0)
        {
            tracing::debug!(yt_dlp_python_version = %expected_version, worker_import_ms = reported_ms, "resident yt-dlp API worker initialized");
        }
        Ok(worker)
    }

    async fn extract(&mut self, raw_url: &str, fast: bool) -> Result<YtDlpInfo, YtDlpError> {
        self.request_id = self.request_id.wrapping_add(1).max(1);
        let request_id = self.request_id;
        let request = serde_json::json!({
            "op": "extract",
            "id": request_id,
            "url": raw_url,
            "fast": fast,
        });
        let request_bytes =
            serde_json::to_vec(&request).map_err(|_| YtDlpError::PythonWorkerProtocol)?;
        let started_at = Instant::now();
        let exchange = async {
            self.stdin.write_all(&request_bytes).await.map_err(|_| ())?;
            self.stdin.write_all(b"\n").await.map_err(|_| ())?;
            self.stdin.flush().await.map_err(|_| ())?;
            read_worker_response(&mut self.stdout).await
        };
        let response_bytes = match timeout(extraction_timeout(), exchange).await {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(_)) => {
                self.stop().await;
                return Err(YtDlpError::PythonWorkerUnavailable);
            }
            Err(_) => {
                self.stop().await;
                return Err(YtDlpError::Timeout(extraction_timeout()));
            }
        };
        let ipc_elapsed = started_at.elapsed();
        wotoha_core::operational_metrics::operational_metrics()
            .observe_yt_dlp_worker_ipc(ipc_elapsed);
        let response: PythonWorkerResponse = match serde_json::from_slice(&response_bytes) {
            Ok(response) => response,
            Err(_) => {
                self.stop().await;
                return Err(YtDlpError::PythonWorkerProtocol);
            }
        };
        if response.id != request_id {
            self.stop().await;
            return Err(YtDlpError::PythonWorkerProtocol);
        }
        if let Some(milliseconds) = response
            .api_extract_ms
            .filter(|value| value.is_finite() && (0.0..=120_000.0).contains(value))
        {
            wotoha_core::operational_metrics::operational_metrics()
                .observe_yt_dlp_worker_api(Duration::from_secs_f64(milliseconds / 1000.0));
        }
        if let Some(profile) = response.profile {
            let mut http_request_details =
                [YtDlpHttpRequestDetail::default(); MAX_YTDLP_HTTP_REQUEST_DETAILS];
            let http_request_detail_count = profile
                .http_request_details
                .len()
                .min(MAX_YTDLP_HTTP_REQUEST_DETAILS);
            for (target, source) in http_request_details
                .iter_mut()
                .zip(profile.http_request_details.iter())
                .take(http_request_detail_count)
            {
                *target = YtDlpHttpRequestDetail {
                    kind: parse_http_request_kind(&source.kind),
                    status_code: source.status_code,
                    open_micros: source.open_us,
                    body_read_micros: source.body_read_us,
                };
            }
            wotoha_core::operational_metrics::operational_metrics().record_yt_dlp_worker_profile(
                YtDlpWorkerProfile {
                    extraction_count: 1,
                    http_requests: profile.http_requests,
                    http_open_micros: profile.http_open_us,
                    http_body_read_micros: profile.http_body_read_us,
                    dns_calls: profile.dns_calls,
                    dns_micros: profile.dns_us,
                    tcp_connect_calls: profile.tcp_connect_calls,
                    tcp_connect_micros: profile.tcp_connect_us,
                    tls_handshakes: profile.tls_handshakes,
                    tls_micros: profile.tls_us,
                    js_solver_calls: profile.js_solver_calls,
                    js_solver_micros: profile.js_solver_us,
                    js_runtime_calls: profile.js_runtime_calls,
                    js_runtime_micros: profile.js_runtime_us,
                    cpu_user_micros: profile.cpu_user_us,
                    cpu_system_micros: profile.cpu_system_us,
                    child_cpu_user_micros: profile.child_cpu_user_us,
                    child_cpu_system_micros: profile.child_cpu_system_us,
                    rss_highwater_kb: profile.rss_highwater_kb,
                    child_rss_highwater_kb: profile.child_rss_highwater_kb,
                    youtube_requests: profile.youtube_requests,
                    media_cdn_requests: profile.media_cdn_requests,
                    other_requests: profile.other_requests,
                    http_request_detail_count: http_request_detail_count as u8,
                    http_request_details,
                    ..YtDlpWorkerProfile::default()
                },
            );
        }
        if response.ok {
            response.info.ok_or(YtDlpError::PythonWorkerProtocol)
        } else {
            Err(YtDlpError::Failed {
                status: "python-worker".to_owned(),
                message: sanitize_python_worker_error(response.error.as_deref()),
            })
        }
    }

    async fn stop(&mut self) {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
    }
}

fn sanitize_python_worker_error(error: Option<&str>) -> String {
    match error {
        Some(
            "Sign in to confirm your age"
            | "Sign in required"
            | "Video unavailable in this region"
            | "HTTP Error 429: Too Many Requests"
            | "Requested format is not available",
        ) => error.expect("matched Some value").to_owned(),
        _ => "yt-dlp Python API extraction failed".to_owned(),
    }
}

fn parse_http_request_kind(kind: &str) -> YtDlpHttpRequestKind {
    match kind {
        "youtube_player_api" => YtDlpHttpRequestKind::YouTubePlayerApi,
        "youtube_client_config" => YtDlpHttpRequestKind::YouTubeClientConfig,
        "youtube_webpage" => YtDlpHttpRequestKind::YouTubeWebpage,
        "youtube_initial_data" => YtDlpHttpRequestKind::YouTubeInitialData,
        "youtube_player_script" => YtDlpHttpRequestKind::YouTubePlayerScript,
        "media_cdn" => YtDlpHttpRequestKind::MediaCdn,
        _ => YtDlpHttpRequestKind::Other,
    }
}

fn python_path() -> Result<PathBuf, YtDlpError> {
    if let Some(path) = env::var_os("WOTOHA_YTDLP_PYTHON_PATH").map(PathBuf::from) {
        if path.is_absolute() && path.is_file() {
            return Ok(path);
        }
        return Err(YtDlpError::PythonWorkerConfiguration);
    }
    [
        Path::new("/usr/bin/python3"),
        Path::new("/usr/local/bin/python3"),
    ]
    .into_iter()
    .find(|path| path.is_file())
    .map(Path::to_path_buf)
    .ok_or(YtDlpError::PythonWorkerConfiguration)
}

async fn read_worker_response<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<Vec<u8>, ()> {
    let mut output = Vec::new();
    loop {
        let (count, newline) = {
            let available = reader.fill_buf().await.map_err(|_| ())?;
            if available.is_empty() {
                return Err(());
            }
            let count = available
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(available.len(), |index| index + 1);
            if output.len().saturating_add(count) > MAX_WORKER_LINE_BYTES {
                return Err(());
            }
            output.extend_from_slice(&available[..count]);
            (count, available[count - 1] == b'\n')
        };
        reader.consume(count);
        if newline {
            output.pop();
            return Ok(output);
        }
    }
}

async fn run_command(
    executable: &Path,
    arguments: Vec<String>,
    timeout_duration: Duration,
) -> Result<Vec<u8>, YtDlpError> {
    run_command_with_limits(
        executable,
        arguments,
        timeout_duration,
        MAX_STDOUT_BYTES,
        MAX_STDERR_BYTES,
    )
    .await
}

async fn run_command_with_limits(
    executable: &Path,
    arguments: Vec<String>,
    timeout_duration: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
) -> Result<Vec<u8>, YtDlpError> {
    let mut command = Command::new(executable);
    command
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for argument in arguments {
        command.arg(argument);
    }
    let started_at = Instant::now();
    let mut child = command.spawn().map_err(|source| YtDlpError::Unavailable {
        path: executable.display().to_string(),
        source,
    })?;
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    let work = async {
        let status = async { child.wait().await.map_err(CappedReadError::Io) };
        tokio::try_join!(
            status,
            read_capped(stdout, stdout_limit, "stdout"),
            read_capped(stderr, stderr_limit, "stderr")
        )
    };
    let (status, stdout, stderr) = match timeout(timeout_duration, work).await {
        Ok(Ok(output)) => output,
        Ok(Err(CappedReadError::Io(source))) => {
            kill_and_reap(&mut child).await;
            wotoha_core::operational_metrics::operational_metrics()
                .observe_yt_dlp_process(started_at.elapsed());
            return Err(YtDlpError::Unavailable {
                path: executable.display().to_string(),
                source,
            });
        }
        Ok(Err(CappedReadError::OutputTooLarge { stream, limit })) => {
            kill_and_reap(&mut child).await;
            wotoha_core::operational_metrics::operational_metrics()
                .observe_yt_dlp_process(started_at.elapsed());
            return Err(YtDlpError::OutputTooLarge { stream, limit });
        }
        Err(_) => {
            kill_and_reap(&mut child).await;
            wotoha_core::operational_metrics::operational_metrics()
                .observe_yt_dlp_process(started_at.elapsed());
            return Err(YtDlpError::Timeout(timeout_duration));
        }
    };
    wotoha_core::operational_metrics::operational_metrics()
        .observe_yt_dlp_process(started_at.elapsed());
    if !status.success() {
        return Err(YtDlpError::Failed {
            status: status
                .code()
                .map_or_else(|| "signal".to_owned(), |code| code.to_string()),
            message: concise_stderr(&stderr),
        });
    }
    Ok(stdout)
}

async fn kill_and_reap(child: &mut Child) {
    let _ = child.kill().await;
    let _ = child.wait().await;
}

enum CappedReadError {
    Io(std::io::Error),
    OutputTooLarge { stream: &'static str, limit: usize },
}

async fn read_capped<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R,
    limit: usize,
    stream: &'static str,
) -> Result<Vec<u8>, CappedReadError> {
    let mut output = Vec::new();
    let mut chunk = [0; 8192];
    loop {
        let count = reader.read(&mut chunk).await.map_err(CappedReadError::Io)?;
        if count == 0 {
            return Ok(output);
        }
        if output.len().saturating_add(count) > limit {
            return Err(CappedReadError::OutputTooLarge { stream, limit });
        }
        output.extend_from_slice(&chunk[..count]);
    }
}

fn arguments(raw_url: &str, cookies: Option<PathBuf>, extractor_args: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "--ignore-config".into(),
        "--dump-single-json".into(),
        "--no-playlist".into(),
        "--skip-download".into(),
        "--no-warnings".into(),
        "--no-progress".into(),
        "--socket-timeout".into(),
        "10".into(),
        "--retries".into(),
        "1".into(),
        "--extractor-retries".into(),
        "1".into(),
        "--format".into(),
        "bestaudio[protocol^=http]/bestaudio/best".into(),
    ];
    if let Some(extractor_args) = extractor_args {
        args.push("--extractor-args".into());
        args.push(extractor_args.to_owned());
    }
    if let Some(cookies) = cookies {
        args.push("--cookies".into());
        args.push(cookies.display().to_string());
    }
    if let Some(deno) = deno_path() {
        args.push("--js-runtimes".into());
        args.push(format!("deno:{}", deno.display()));
    }
    args.push("--".into());
    args.push(raw_url.to_owned());
    args
}

fn should_retry_with_full_format_discovery(error: &YtDlpError) -> bool {
    let YtDlpError::Failed { message, .. } = error else {
        return matches!(error, YtDlpError::MissingUrl);
    };
    if error.playback_failure_kind() != Some(PlaybackFailureKind::TemporarySource) {
        return false;
    }

    let message = message.to_ascii_lowercase();
    if [
        "http error ",
        "http status code",
        "too many requests",
        "rate limit",
        "rate-limit",
        "not a bot",
    ]
    .iter()
    .any(|marker| message.contains(marker))
    {
        return false;
    }

    message.contains("requested format is not available")
        || message.contains("no video formats found")
        || message.contains("no formats found")
}

fn track_request_from_info(
    requested_url: &str,
    info: YtDlpInfo,
) -> Result<TrackRequest, YtDlpError> {
    let stream_url = info
        .url
        .filter(|url| !url.is_empty())
        .ok_or(YtDlpError::MissingUrl)?;
    let canonical_url = info
        .webpage_url
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| format!("https://www.youtube.com/watch?v={}", info.id));
    let headers = sanitize_headers(info.http_headers)?;
    let expires_at_unix = url_query_u64(&stream_url, "expire");
    let content_length = info
        .filesize
        .or(info.filesize_approx)
        .or_else(|| url_query_u64(&stream_url, "clen"));
    let is_hls = info.is_live
        || info.protocol.as_deref().is_some_and(|p| p.contains("m3u8"))
        || stream_url.contains(".m3u8");
    let is_googlevideo = is_googlevideo_url(&stream_url);
    let prepared = if is_hls {
        PreparedSource::hls(stream_url, headers, expires_at_unix)
    } else {
        PreparedSource::http_with_range_mode(
            stream_url,
            headers,
            content_length,
            is_googlevideo.then_some(YOUTUBE_RANGE_CHUNK_SIZE),
            if is_googlevideo {
                PreparedRangeMode::QueryParam
            } else {
                PreparedRangeMode::Header
            },
            expires_at_unix,
        )
    };
    let author = info
        .uploader
        .filter(|v| !v.is_empty())
        .or_else(|| info.channel.filter(|v| !v.is_empty()))
        .unwrap_or_else(|| "YouTube".to_owned());
    let title = if info.title.is_empty() {
        canonical_url.clone()
    } else {
        info.title
    };
    let duration = if info.is_live {
        None
    } else {
        info.duration
            .filter(|v| v.is_finite() && *v > 0.0 && *v <= 24.0 * 60.0 * 60.0)
            .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok())
    };
    Ok(TrackRequest::new(
        "youtube",
        format!("youtube:video:{}", info.id),
        requested_url.to_owned(),
        canonical_url.clone(),
        canonical_url.clone(),
        prepared,
        TrackMetadata::new(
            title,
            author,
            canonical_url,
            info.thumbnail.map(Arc::<str>::from),
            duration,
        ),
    ))
}

fn sanitize_headers(
    headers: Option<HashMap<String, String>>,
) -> Result<Vec<PreparedHeader>, YtDlpError> {
    const ALLOWED: &[&str] = &[
        "user-agent",
        "referer",
        "origin",
        "accept",
        "accept-language",
    ];
    let mut result = Vec::new();
    for (name, value) in headers.unwrap_or_default() {
        let normalized_name = name.to_ascii_lowercase();
        let is_sensitive = matches!(
            normalized_name.as_str(),
            "cookie" | "authorization" | "proxy-authorization" | "proxy-connection"
        ) || normalized_name.starts_with("proxy-");
        if is_sensitive || !ALLOWED.iter().any(|allowed| normalized_name == *allowed) {
            continue;
        }
        if value.contains(['\r', '\n']) || value.len() > MAX_HEADER_VALUE_BYTES {
            return Err(YtDlpError::UnsafeHeaders);
        }
        if result.len() >= MAX_HEADERS {
            return Err(YtDlpError::UnsafeHeaders);
        }
        result.push(PreparedHeader::new(name, value));
    }
    Ok(result)
}

pub fn resolve_ytdlp_path() -> Result<PathBuf, YtDlpError> {
    resolve_ytdlp_path_with_candidates(
        env::var_os("WOTOHA_YTDLP_PATH").map(PathBuf::from),
        Path::new("/wotoha/tools/yt-dlp"),
        Path::new("/wotoha/tools/yt-dlp.sha256"),
        Path::new("/app/tools/yt-dlp-fallback"),
        Path::new("/opt/wotoha/bin/yt-dlp"),
    )
}

fn yt_dlp_path() -> Result<PathBuf, YtDlpError> {
    resolve_ytdlp_path()
}

fn resolve_ytdlp_path_with_candidates(
    explicit: Option<PathBuf>,
    managed: &Path,
    managed_digest: &Path,
    fallback: &Path,
    legacy: &Path,
) -> Result<PathBuf, YtDlpError> {
    let path = if let Some(path) = explicit {
        path
    } else if managed.is_file() {
        match verified_managed_ytdlp_override(managed, managed_digest) {
            Ok(Some(path)) => path,
            Ok(None) => unreachable!("managed yt-dlp was checked as a regular file"),
            Err(error) => {
                eprintln!(
                    "wotoha: ignoring invalid managed yt-dlp override at {} ({error}); continuing with fallback resolution",
                    managed.display()
                );
                [fallback, legacy]
                    .into_iter()
                    .find(|candidate| candidate.is_file())
                    .ok_or(YtDlpError::NotConfigured)?
                    .to_path_buf()
            }
        }
    } else {
        [fallback, legacy]
            .into_iter()
            .find(|candidate| candidate.is_file())
            .ok_or(YtDlpError::NotConfigured)?
            .to_path_buf()
    };
    if !path.is_absolute() {
        return Err(YtDlpError::RelativePath(path.display().to_string()));
    }
    Ok(path.to_path_buf())
}

fn verified_managed_ytdlp_override(
    path: &Path,
    digest_path: &Path,
) -> Result<Option<PathBuf>, YtDlpError> {
    if !path.is_file() {
        return Ok(None);
    }
    let expected = std::fs::read_to_string(digest_path)
        .ok()
        .and_then(|contents| contents.split_whitespace().next().map(str::to_owned));
    let actual = std::fs::read(path)
        .ok()
        .map(|contents| format!("{:x}", Sha256::digest(contents)))
        .unwrap_or_default();
    if expected.as_deref() == Some(actual.as_str()) {
        Ok(Some(path.to_path_buf()))
    } else {
        Err(YtDlpError::UnverifiedManagedTool {
            path: path.display().to_string(),
        })
    }
}

fn cookies_file() -> Result<Option<PathBuf>, YtDlpError> {
    let Some(path) = env::var_os("WOTOHA_YTDLP_COOKIES_FILE").map(PathBuf::from) else {
        return Ok(None);
    };
    if !path.is_absolute() || !path.is_file() {
        return Err(YtDlpError::CookiesFile(path.display().to_string()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let mode = std::fs::metadata(&path)
            .map_err(|_| YtDlpError::CookiesFile(path.display().to_string()))?
            .mode();
        if mode & 0o077 != 0 {
            return Err(YtDlpError::CookiesFile(path.display().to_string()));
        }
    }
    Ok(Some(path))
}
fn deno_path() -> Option<PathBuf> {
    env::var_os("WOTOHA_DENO_PATH")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute() && path.is_file())
        .or_else(|| {
            [
                PathBuf::from("/app/tools/deno"),
                PathBuf::from("/opt/wotoha/bin/deno"),
            ]
            .into_iter()
            .find(|path| path.is_file())
        })
}
fn extraction_timeout() -> Duration {
    env::var("WOTOHA_YTDLP_TIMEOUT_SECONDS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| (5..=120).contains(v))
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_TIMEOUT)
}
fn concurrency() -> usize {
    env::var("WOTOHA_YTDLP_CONCURRENCY")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v| (1..=8).contains(v))
        .unwrap_or(DEFAULT_CONCURRENCY)
}
fn concise_stderr(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= 500 {
        compact
    } else {
        format!("{}…", compact.chars().take(500).collect::<String>())
    }
}

fn classify_failure_message(message: &str) -> PlaybackFailureKind {
    let normalized = message.to_ascii_lowercase();
    if normalized.contains("confirm your age")
        || normalized.contains("age-restricted")
        || normalized.contains("age restricted")
    {
        return PlaybackFailureKind::AgeVerificationRequired;
    }
    if normalized.contains("sign in")
        || normalized.contains("authentication required")
        || normalized.contains("login required")
        || normalized.contains("members-only")
        || normalized.contains("members only")
    {
        return PlaybackFailureKind::AuthenticationRequired;
    }
    if normalized.contains("private video")
        || normalized.contains("video is private")
        || normalized.contains("video unavailable")
        || normalized.contains("not available in your country")
        || normalized.contains("geo-restricted")
        || normalized.contains("geoblocked")
    {
        return PlaybackFailureKind::AccessRestricted;
    }
    PlaybackFailureKind::TemporarySource
}
fn url_query_u64(raw_url: &str, key: &str) -> Option<u64> {
    reqwest::Url::parse(raw_url)
        .ok()?
        .query_pairs()
        .find(|(candidate, _)| candidate == key)
        .and_then(|(_, value)| value.parse().ok())
}
fn is_googlevideo_url(raw_url: &str) -> bool {
    reqwest::Url::parse(raw_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .is_some_and(|host| host == "googlevideo.com" || host.ends_with(".googlevideo.com"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_worker_is_opt_in_and_rejects_unknown_backends() {
        assert!(!parse_python_worker_backend(None).unwrap());
        assert!(!parse_python_worker_backend(Some("cli")).unwrap());
        assert!(parse_python_worker_backend(Some("python-worker")).unwrap());
        assert!(matches!(
            parse_python_worker_backend(Some("unknown")),
            Err(YtDlpError::PythonWorkerConfiguration)
        ));
    }

    #[test]
    fn internal_request_profile_labels_are_low_cardinality_and_safe() {
        assert_eq!(
            parse_http_request_kind("youtube_player_api"),
            YtDlpHttpRequestKind::YouTubePlayerApi
        );
        assert_eq!(
            parse_http_request_kind("media_cdn"),
            YtDlpHttpRequestKind::MediaCdn
        );
        assert_eq!(
            parse_http_request_kind("https://example.test/path?signature=secret"),
            YtDlpHttpRequestKind::Other
        );
    }

    #[test]
    fn python_worker_error_payloads_are_allowlisted_before_logging_or_ui_use() {
        assert_eq!(
            sanitize_python_worker_error(Some("Sign in to confirm your age")),
            "Sign in to confirm your age"
        );
        assert_eq!(
            sanitize_python_worker_error(Some("https://media.test/path?signature=secret")),
            "yt-dlp Python API extraction failed"
        );
    }

    #[tokio::test]
    async fn python_worker_protocol_reads_one_bounded_json_line() {
        let (mut writer, reader) = tokio::io::duplex(128);
        let write = tokio::spawn(async move {
            writer.write_all(b"{\"id\":1}\n{\"id\":2}\n").await.unwrap();
        });
        let mut reader = BufReader::new(reader);
        assert_eq!(
            read_worker_response(&mut reader).await.unwrap(),
            b"{\"id\":1}"
        );
        assert_eq!(
            read_worker_response(&mut reader).await.unwrap(),
            b"{\"id\":2}"
        );
        write.await.unwrap();
    }

    #[tokio::test]
    async fn python_worker_protocol_rejects_oversized_lines() {
        let (mut writer, reader) = tokio::io::duplex(MAX_WORKER_LINE_BYTES + 8);
        let write = tokio::spawn(async move {
            let oversized = vec![b'x'; MAX_WORKER_LINE_BYTES + 1];
            writer.write_all(&oversized).await.unwrap();
            writer.write_all(b"\n").await.unwrap();
        });
        let mut reader = BufReader::new(reader);
        assert!(read_worker_response(&mut reader).await.is_err());
        write.await.unwrap();
    }

    #[tokio::test]
    async fn python_worker_pool_leases_distinct_slots_for_parallel_requests() {
        let pool = Arc::new(PythonWorkerPool {
            workers: vec![Arc::new(Mutex::new(None)), Arc::new(Mutex::new(None))],
            next_worker: AtomicUsize::new(0),
        });
        let first = pool.lock_available_worker().await;
        let second = pool.lock_available_worker().await;
        assert!(!std::ptr::eq(&*first, &*second));

        let (sender, receiver) = tokio::sync::oneshot::channel();
        let waiting_pool = Arc::clone(&pool);
        let waiting = tokio::spawn(async move {
            let guard = waiting_pool.lock_available_worker().await;
            let _ = sender.send(());
            drop(guard);
        });
        assert!(timeout(Duration::from_millis(10), receiver).await.is_err());
        drop(first);
        waiting.await.unwrap();
    }

    #[test]
    fn forces_safe_single_track_arguments() {
        let args = arguments("https://www.youtube.com/watch?v=x", None, None);
        assert!(
            args.windows(2)
                .any(|a| a == ["--ignore-config", "--dump-single-json"])
        );
        assert!(args.contains(&"--no-playlist".to_owned()));
        assert_eq!(args[args.len() - 2], "--");
        assert_eq!(args.last().unwrap(), "https://www.youtube.com/watch?v=x");
    }

    #[test]
    fn uses_supported_manifest_skip_for_the_fast_youtube_pass() {
        let args = arguments(
            "https://www.youtube.com/watch?v=x",
            None,
            Some(FAST_YOUTUBE_EXTRACTOR_ARGS),
        );
        assert!(
            args.windows(2)
                .any(|pair| { pair == ["--extractor-args", "youtube:skip=hls,dash"] })
        );
    }

    #[test]
    fn conservative_format_retry_is_limited_to_missing_format_errors() {
        assert!(should_retry_with_full_format_discovery(
            &YtDlpError::MissingUrl
        ));
        for message in [
            "ERROR: Requested format is not available",
            "ERROR: No video formats found",
            "ERROR: no formats found",
        ] {
            let error = YtDlpError::Failed {
                status: "1".into(),
                message: message.into(),
            };
            assert!(should_retry_with_full_format_discovery(&error), "{message}");
        }
    }

    #[test]
    fn conservative_format_retry_never_retries_access_or_rate_limits() {
        for message in [
            "HTTP Error 401: Unauthorized",
            "HTTP Error 403: Forbidden",
            "HTTP Error 404: Not Found",
            "HTTP Error 410: Gone",
            "HTTP Error 429: Too Many Requests",
            "HTTP Error 503: Service Unavailable",
            "rate limit reached; requested format is not available",
            "Sign in to confirm your age",
            "Sign in to confirm you are not a bot",
            "This video is private",
        ] {
            let error = YtDlpError::Failed {
                status: "1".into(),
                message: message.into(),
            };
            assert!(
                !should_retry_with_full_format_discovery(&error),
                "{message}"
            );
        }
        assert!(!should_retry_with_full_format_discovery(
            &YtDlpError::Timeout(Duration::from_secs(25))
        ));
    }

    #[test]
    fn python_worker_cli_fallback_is_limited_to_worker_startup_or_ipc_failures() {
        assert!(should_fallback_to_cli(&YtDlpError::PythonWorkerUnavailable));
        assert!(should_fallback_to_cli(&YtDlpError::PythonWorkerProtocol));
        assert!(should_fallback_to_cli(
            &YtDlpError::PythonWorkerConfiguration
        ));
        assert!(!should_fallback_to_cli(&YtDlpError::Timeout(
            Duration::from_secs(25)
        )));
        assert!(!should_fallback_to_cli(&YtDlpError::Failed {
            status: "python-worker".into(),
            message: "Sign in to confirm your age".into(),
        }));
    }

    #[test]
    fn classifies_yt_dlp_access_failures_without_exposing_stderr_to_ui() {
        let age = YtDlpError::Failed {
            status: "1".into(),
            message: "ERROR: Sign in to confirm your age".into(),
        };
        assert_eq!(
            age.playback_failure_kind(),
            Some(PlaybackFailureKind::AgeVerificationRequired)
        );

        let private = YtDlpError::Failed {
            status: "1".into(),
            message: "ERROR: This video is private".into(),
        };
        assert_eq!(
            private.playback_failure_kind(),
            Some(PlaybackFailureKind::AccessRestricted)
        );
    }
    #[test]
    fn drops_secret_headers_and_rejects_injected_retained_headers() {
        let mut headers = HashMap::new();
        headers.insert("Cookie".into(), "secret".into());
        headers.insert("Authorization".into(), "Bearer secret".into());
        headers.insert("Proxy-Authorization".into(), "Basic secret".into());
        assert!(sanitize_headers(Some(headers)).unwrap().is_empty());
        let mut headers = HashMap::new();
        headers.insert("User-Agent".into(), "a\nb".into());
        assert!(matches!(
            sanitize_headers(Some(headers)),
            Err(YtDlpError::UnsafeHeaders)
        ));
    }
    #[test]
    fn drops_realistic_unknown_headers_and_retains_playback_headers() {
        let mut headers = HashMap::new();
        headers.insert("User-Agent".into(), "Mozilla/5.0".into());
        headers.insert("Referer".into(), "https://www.youtube.com/".into());
        headers.insert("Sec-Fetch-Mode".into(), "navigate".into());
        headers.insert("Sec-Fetch-Site".into(), "same-origin".into());

        let retained = sanitize_headers(Some(headers)).unwrap();
        assert_eq!(retained.len(), 2);
        assert!(
            retained
                .iter()
                .any(|header| header.name.eq_ignore_ascii_case("user-agent"))
        );
        assert!(
            retained
                .iter()
                .any(|header| header.name.eq_ignore_ascii_case("referer"))
        );
        assert!(
            !retained
                .iter()
                .any(|header| header.name.as_ref().starts_with("Sec-Fetch-"))
        );
    }
    #[test]
    fn recognizes_googlevideo_hosts_only() {
        assert!(is_googlevideo_url(
            "https://rr1---sn.example.googlevideo.com/videoplayback"
        ));
        assert!(!is_googlevideo_url(
            "https://googlevideo.com.example.test/videoplayback"
        ));
    }

    fn resolution_directory(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "wotoha-ytdlp-resolution-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    fn write_digest(path: &Path, digest_path: &Path) {
        let contents = std::fs::read(path).unwrap();
        std::fs::write(
            digest_path,
            format!("{:x}  {}\n", Sha256::digest(contents), path.display()),
        )
        .unwrap();
    }

    #[test]
    fn explicit_ytdlp_override_wins_and_must_be_absolute() {
        let directory = resolution_directory("explicit");
        let managed = directory.join("managed");
        let digest = directory.join("managed.sha256");
        let fallback = directory.join("fallback");
        let legacy = directory.join("legacy");
        std::fs::write(&managed, b"managed").unwrap();
        write_digest(&managed, &digest);
        std::fs::write(&fallback, b"fallback").unwrap();
        std::fs::write(&legacy, b"legacy").unwrap();

        assert_eq!(
            resolve_ytdlp_path_with_candidates(
                Some(directory.join("administrator")),
                &managed,
                &digest,
                &fallback,
                &legacy,
            )
            .unwrap(),
            directory.join("administrator")
        );
        assert!(matches!(
            resolve_ytdlp_path_with_candidates(
                Some(PathBuf::from("relative/yt-dlp")),
                &managed,
                &digest,
                &fallback,
                &legacy,
            ),
            Err(YtDlpError::RelativePath(_))
        ));
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn valid_managed_override_beats_image_and_legacy_fallbacks() {
        let directory = resolution_directory("managed");
        let managed = directory.join("managed");
        let digest = directory.join("managed.sha256");
        let fallback = directory.join("fallback");
        let legacy = directory.join("legacy");
        std::fs::write(&managed, b"managed").unwrap();
        write_digest(&managed, &digest);
        std::fs::write(&fallback, b"fallback").unwrap();
        std::fs::write(&legacy, b"legacy").unwrap();

        assert_eq!(
            resolve_ytdlp_path_with_candidates(None, &managed, &digest, &fallback, &legacy)
                .unwrap(),
            managed
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn no_managed_override_uses_image_fallback_before_legacy() {
        let directory = resolution_directory("fallback");
        let managed = directory.join("missing-managed");
        let digest = directory.join("missing-managed.sha256");
        let fallback = directory.join("fallback");
        let legacy = directory.join("legacy");
        std::fs::write(&fallback, b"fallback").unwrap();
        std::fs::write(&legacy, b"legacy").unwrap();

        assert_eq!(
            resolve_ytdlp_path_with_candidates(None, &managed, &digest, &fallback, &legacy)
                .unwrap(),
            fallback
        );
        let _ = std::fs::remove_file(&fallback);
        assert_eq!(
            resolve_ytdlp_path_with_candidates(None, &managed, &digest, &fallback, &legacy)
                .unwrap(),
            legacy
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn invalid_managed_digest_uses_image_fallback() {
        let directory = resolution_directory("invalid");
        let managed = directory.join("managed");
        let digest = directory.join("managed.sha256");
        let fallback = directory.join("fallback");
        let legacy = directory.join("legacy");
        std::fs::write(&managed, b"managed").unwrap();
        std::fs::write(
            &digest,
            format!("{}  {}\n", "0".repeat(64), managed.display()),
        )
        .unwrap();
        std::fs::write(&fallback, b"fallback").unwrap();
        std::fs::write(&legacy, b"legacy").unwrap();

        assert_eq!(
            resolve_ytdlp_path_with_candidates(None, &managed, &digest, &fallback, &legacy)
                .unwrap(),
            fallback
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn resolver_order_is_deterministic_when_candidates_are_repeated() {
        let directory = resolution_directory("deterministic");
        let managed = directory.join("managed");
        let digest = directory.join("managed.sha256");
        let fallback = directory.join("fallback");
        let legacy = directory.join("legacy");
        std::fs::write(&fallback, b"fallback").unwrap();

        for _ in 0..8 {
            assert_eq!(
                resolve_ytdlp_path_with_candidates(None, &managed, &digest, &fallback, &legacy)
                    .unwrap(),
                fallback
            );
        }
        let _ = std::fs::remove_dir_all(directory);
    }

    #[cfg(unix)]
    fn shell_command(script: &str) -> (PathBuf, Vec<String>) {
        (PathBuf::from("/bin/sh"), vec!["-c".into(), script.into()])
    }

    #[cfg(windows)]
    fn shell_command(script: &str) -> (PathBuf, Vec<String>) {
        let executable = std::env::var_os("COMSPEC")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows\System32\cmd.exe"));
        (
            executable,
            vec!["/D".into(), "/S".into(), "/C".into(), script.into()],
        )
    }

    #[cfg(unix)]
    const OUTPUT_SCRIPT: &str = "printf 12345";
    #[cfg(windows)]
    const OUTPUT_SCRIPT: &str = "<nul set /p =12345";
    #[cfg(unix)]
    const FAILURE_SCRIPT: &str = "printf 'extraction failed\\n' >&2; exit 7";
    #[cfg(windows)]
    const FAILURE_SCRIPT: &str = "echo extraction failed 1>&2 & exit /b 7";
    #[cfg(unix)]
    const UNBOUNDED_OUTPUT_SCRIPT: &str = "while :; do printf 0123456789; done";
    #[cfg(windows)]
    const UNBOUNDED_OUTPUT_SCRIPT: &str = "for /L %i in (1,1,1000000) do <nul set /p =0123456789";
    #[cfg(unix)]
    fn timeout_command() -> (PathBuf, Vec<String>) {
        (PathBuf::from("/bin/sleep"), vec!["30".into()])
    }

    #[cfg(windows)]
    fn timeout_command() -> (PathBuf, Vec<String>) {
        (
            PathBuf::from(r"C:\Windows\System32\PING.EXE"),
            vec!["-n".into(), "30".into(), "127.0.0.1".into()],
        )
    }

    #[tokio::test]
    async fn rejects_capped_command_output() {
        let (executable, arguments) = shell_command(OUTPUT_SCRIPT);
        let result =
            run_command_with_limits(&executable, arguments, Duration::from_secs(2), 4, 1024).await;
        assert!(matches!(
            result,
            Err(YtDlpError::OutputTooLarge {
                stream: "stdout",
                limit: 4
            })
        ));
    }

    #[tokio::test]
    async fn kills_and_reaps_an_unbounded_output_producer_promptly() {
        let (executable, arguments) = shell_command(UNBOUNDED_OUTPUT_SCRIPT);
        let started = std::time::Instant::now();
        let result =
            run_command_with_limits(&executable, arguments, Duration::from_secs(5), 64, 1024).await;

        assert!(matches!(
            result,
            Err(YtDlpError::OutputTooLarge {
                stream: "stdout",
                limit: 64
            })
        ));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "output cap must terminate the child without waiting for the command timeout"
        );
    }

    #[tokio::test]
    async fn reports_nonzero_status_and_stderr() {
        let (executable, arguments) = shell_command(FAILURE_SCRIPT);
        let result = run_command(&executable, arguments, Duration::from_secs(2)).await;
        assert!(matches!(
            result,
            Err(YtDlpError::Failed { status, message })
                if status == "7" && message == "extraction failed"
        ));
    }

    #[tokio::test]
    async fn kills_and_reaps_a_timed_out_command() {
        let (executable, arguments) = timeout_command();
        let result = run_command(&executable, arguments, Duration::from_millis(20)).await;
        assert!(matches!(result, Err(YtDlpError::Timeout(_))));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn reads_json_from_a_fake_executable() {
        let path =
            std::env::temp_dir().join(format!("wotoha-fake-ytdlp-{}.cmd", std::process::id()));
        std::fs::write(
            &path,
            "@echo {\"id\":\"abc\",\"url\":\"https://example.test\"}\r\n",
        )
        .unwrap();
        let output = run_command(&path, Vec::new(), Duration::from_secs(2))
            .await
            .unwrap();
        let info: YtDlpInfo = serde_json::from_slice(&output).unwrap();
        assert_eq!(info.id, "abc");
        let _ = std::fs::remove_file(path);
    }
}
