use std::{collections::HashMap, env, path::PathBuf, process::Stdio, sync::Arc, time::Duration};

use serde::Deserialize;
use tokio::{process::Command, time::timeout};
use wotoha_core::{PreparedHeader, PreparedRangeMode, PreparedSource, TrackMetadata, TrackRequest};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(25);
const YOUTUBE_RANGE_CHUNK_SIZE: u64 = 11_862_014;

#[derive(Debug, thiserror::Error)]
pub enum YtDlpError {
    #[error("yt-dlp is unavailable at {path}: {source}")]
    Unavailable {
        path: String,
        source: std::io::Error,
    },
    #[error("yt-dlp timed out after {0:?}")]
    Timeout(Duration),
    #[error("yt-dlp failed with status {status}: {message}")]
    Failed { status: String, message: String },
    #[error("yt-dlp returned invalid JSON: {0}")]
    InvalidJson(serde_json::Error),
    #[error("yt-dlp did not return a playable URL")]
    MissingUrl,
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
    let info = extract(raw_url).await?;
    track_request_from_info(raw_url, info)
}

pub async fn refresh(request: &TrackRequest) -> Result<TrackRequest, YtDlpError> {
    let info = extract(request.canonical_url.as_ref()).await?;
    let mut refreshed = track_request_from_info(request.requested_url.as_ref(), info)?;
    refreshed.metadata = request.metadata.clone();
    Ok(refreshed)
}

async fn extract(raw_url: &str) -> Result<YtDlpInfo, YtDlpError> {
    let executable = yt_dlp_path();
    let timeout_duration = extraction_timeout();
    let mut command = Command::new(&executable);
    command
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .arg("--dump-single-json")
        .arg("--no-playlist")
        .arg("--skip-download")
        .arg("--no-warnings")
        .arg("--no-progress")
        .arg("--socket-timeout")
        .arg("10")
        .arg("--retries")
        .arg("1")
        .arg("--extractor-retries")
        .arg("1")
        .arg("--format")
        .arg("bestaudio[protocol^=http]/bestaudio/best");

    if let Some(config_path) = yt_dlp_config_path() {
        command.arg("--config-locations").arg(config_path);
    } else {
        command.arg("--ignore-config");
    }
    if let Some(deno_path) = deno_path() {
        command
            .arg("--js-runtimes")
            .arg(format!("deno:{}", deno_path.display()));
    }

    command.arg(raw_url);
    let output = match timeout(timeout_duration, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(source)) => {
            return Err(YtDlpError::Unavailable {
                path: executable.display().to_string(),
                source,
            });
        }
        Err(_) => return Err(YtDlpError::Timeout(timeout_duration)),
    };

    if !output.status.success() {
        return Err(YtDlpError::Failed {
            status: output
                .status
                .code()
                .map_or_else(|| "signal".to_owned(), |code| code.to_string()),
            message: concise_stderr(&output.stderr),
        });
    }

    serde_json::from_slice(&output.stdout).map_err(YtDlpError::InvalidJson)
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
    let headers = info
        .http_headers
        .unwrap_or_default()
        .into_iter()
        .map(|(name, value)| PreparedHeader::new(name, value))
        .collect::<Vec<_>>();
    let expires_at_unix = url_query_u64(&stream_url, "expire");
    let content_length = info
        .filesize
        .or(info.filesize_approx)
        .or_else(|| url_query_u64(&stream_url, "clen"));
    let is_hls = info.is_live
        || info
            .protocol
            .as_deref()
            .is_some_and(|protocol| protocol.contains("m3u8"))
        || stream_url.contains(".m3u8");
    let is_googlevideo = is_googlevideo_url(&stream_url);
    let prepared = if is_hls {
        PreparedSource::hls(stream_url, headers, expires_at_unix)
    } else {
        let range_mode = is_googlevideo
            .then_some(PreparedRangeMode::QueryParam)
            .unwrap_or(PreparedRangeMode::Header);
        PreparedSource::http_with_range_mode(
            stream_url,
            headers,
            content_length,
            is_googlevideo.then_some(YOUTUBE_RANGE_CHUNK_SIZE),
            range_mode,
            expires_at_unix,
        )
    };
    let author = info
        .uploader
        .filter(|value| !value.is_empty())
        .or_else(|| info.channel.filter(|value| !value.is_empty()))
        .unwrap_or_else(|| "YouTube".to_owned());
    let title = (!info.title.is_empty())
        .then_some(info.title)
        .unwrap_or_else(|| canonical_url.clone());
    let duration = (!info.is_live)
        .then(|| {
            info.duration
                .filter(|value| value.is_finite() && *value > 0.0)
        })
        .flatten()
        .map(Duration::from_secs_f64);

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

fn yt_dlp_path() -> PathBuf {
    env::var_os("WOTOHA_YTDLP_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let managed = PathBuf::from("/opt/wotoha/bin/yt-dlp");
            managed
                .exists()
                .then_some(managed)
                .unwrap_or_else(|| PathBuf::from("yt-dlp"))
        })
}

fn deno_path() -> Option<PathBuf> {
    env::var_os("WOTOHA_DENO_PATH")
        .map(PathBuf::from)
        .or_else(|| {
            let managed = PathBuf::from("/opt/wotoha/bin/deno");
            managed.exists().then_some(managed)
        })
        .or_else(|| command_on_path("deno"))
}

fn yt_dlp_config_path() -> Option<PathBuf> {
    env::var_os("WOTOHA_YTDLP_CONFIG")
        .map(PathBuf::from)
        .or_else(|| {
            let managed = PathBuf::from("/etc/wotoha/yt-dlp.conf");
            managed.is_file().then_some(managed)
        })
}

fn extraction_timeout() -> Duration {
    env::var("WOTOHA_YTDLP_TIMEOUT_SECONDS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|seconds| (5..=120).contains(seconds))
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_TIMEOUT)
}

fn command_on_path(name: &str) -> Option<PathBuf> {
    env::var_os("PATH").and_then(|paths| {
        env::split_paths(&paths)
            .map(|directory| directory.join(name))
            .find(|candidate| candidate.is_file())
    })
}

fn concise_stderr(stderr: &[u8]) -> String {
    const MAX_CHARS: usize = 500;
    let text = String::from_utf8_lossy(stderr);
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= MAX_CHARS {
        compact
    } else {
        format!("{}…", compact.chars().take(MAX_CHARS).collect::<String>())
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
    fn truncates_extractor_stderr() {
        let output = concise_stderr(&vec![b'x'; 600]);
        assert_eq!(output.chars().count(), 501);
        assert!(output.ends_with('…'));
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
}
