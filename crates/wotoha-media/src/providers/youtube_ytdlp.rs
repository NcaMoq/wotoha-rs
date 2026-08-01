use std::{
    collections::HashMap,
    env,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, OnceLock},
    time::Duration,
};

use serde::Deserialize;
use tokio::{
    io::AsyncReadExt,
    process::{Child, Command},
    sync::Semaphore,
    time::timeout,
};
use wotoha_core::{PreparedHeader, PreparedRangeMode, PreparedSource, TrackMetadata, TrackRequest};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(25);
const QUEUE_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_CONCURRENCY: usize = 2;
const MAX_STDOUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_STDERR_BYTES: usize = 64 * 1024;
const MAX_HEADERS: usize = 8;
const MAX_HEADER_VALUE_BYTES: usize = 1024;
const YOUTUBE_RANGE_CHUNK_SIZE: u64 = 11_862_014;

static YTDLP_SLOTS: OnceLock<Semaphore> = OnceLock::new();

#[derive(Debug, thiserror::Error)]
pub enum YtDlpError {
    #[error("yt-dlp executable is not configured")]
    NotConfigured,
    #[error("yt-dlp path must be absolute: {0}")]
    RelativePath(String),
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

pub async fn probe(raw_url: &str) -> Result<TrackRequest, YtDlpError> {
    track_request_from_info(raw_url, extract(raw_url).await?)
}

pub async fn refresh(request: &TrackRequest) -> Result<TrackRequest, YtDlpError> {
    let mut refreshed = track_request_from_info(
        request.requested_url.as_ref(),
        extract(request.canonical_url.as_ref()).await?,
    )?;
    refreshed.metadata = request.metadata.clone();
    Ok(refreshed)
}

async fn extract(raw_url: &str) -> Result<YtDlpInfo, YtDlpError> {
    let slots = YTDLP_SLOTS.get_or_init(|| Semaphore::new(concurrency()));
    let _permit = timeout(QUEUE_TIMEOUT, slots.acquire())
        .await
        .map_err(|_| YtDlpError::Busy(QUEUE_TIMEOUT))?
        .expect("yt-dlp semaphore stays open");
    let executable = yt_dlp_path()?;
    let stdout = run_command(
        &executable,
        arguments(raw_url, cookies_file()?),
        extraction_timeout(),
    )
    .await?;
    serde_json::from_slice(&stdout).map_err(YtDlpError::InvalidJson)
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
            return Err(YtDlpError::Unavailable {
                path: executable.display().to_string(),
                source,
            });
        }
        Ok(Err(CappedReadError::OutputTooLarge { stream, limit })) => {
            kill_and_reap(&mut child).await;
            return Err(YtDlpError::OutputTooLarge { stream, limit });
        }
        Err(_) => {
            kill_and_reap(&mut child).await;
            return Err(YtDlpError::Timeout(timeout_duration));
        }
    };
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

fn arguments(raw_url: &str, cookies: Option<PathBuf>) -> Vec<String> {
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
            .filter(|v| v.is_finite() && *v > 0.0)
            .map(Duration::from_secs_f64)
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

fn yt_dlp_path() -> Result<PathBuf, YtDlpError> {
    let path = env::var_os("WOTOHA_YTDLP_PATH")
        .map(PathBuf::from)
        .or_else(|| {
            let managed = PathBuf::from("/opt/wotoha/bin/yt-dlp");
            managed.is_file().then_some(managed)
        })
        .ok_or(YtDlpError::NotConfigured)?;
    if !path.is_absolute() {
        return Err(YtDlpError::RelativePath(path.display().to_string()));
    }
    Ok(path)
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
    fn forces_safe_single_track_arguments() {
        let args = arguments("https://www.youtube.com/watch?v=x", None);
        assert!(
            args.windows(2)
                .any(|a| a == ["--ignore-config", "--dump-single-json"])
        );
        assert!(args.contains(&"--no-playlist".to_owned()));
        assert_eq!(args[args.len() - 2], "--");
        assert_eq!(args.last().unwrap(), "https://www.youtube.com/watch?v=x");
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
