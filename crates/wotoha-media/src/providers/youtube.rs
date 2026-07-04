use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use regex::Regex;
use reqwest::{Client, Url};
use rusty_ytdl::{
    Video, VideoFormat, VideoOptions, VideoQuality, VideoSearchOptions, choose_format,
};
use serde::Deserialize;
use wotoha_core::{PreparedHeader, PreparedRangeMode, PreparedSource, TrackMetadata, TrackRequest};

use crate::{ResolveError, provider::MediaProvider};

const ANDROID_VR_CLIENT_VERSION: &str = "1.60.19";
const ANDROID_VR_USER_AGENT: &str = "com.google.android.apps.youtube.vr.oculus/1.60.19 (Linux; U; Android 12L; eureka-user Build/SQ3A.220605.009.A1) gzip";
const YOUTUBE_RANGE_CHUNK_SIZE: u64 = 11_862_014;
const YOUTUBE_ANDROID_VR_FAST_PATH_TIMEOUT: Duration = Duration::from_millis(900);

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
                    "YouTube Android VR fast path failed; falling back to rusty_ytdl"
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

        let expires_at_unix = format_url_expiry(format.stream_url.as_ref());
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
            Ok(None) => return Ok(None),
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "YouTube playback refresh failed; falling back to full probe"
                );
                return Ok(None);
            }
        };

        Ok(Some(track_request_with_format(request, format)))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ChosenFormat {
    stream_url: String,
    content_length: Option<String>,
    is_hls: bool,
    headers: Vec<PreparedHeader>,
    range_chunk_size: Option<u64>,
}

fn track_request_with_format(request: &TrackRequest, format: ChosenFormat) -> TrackRequest {
    let expires_at_unix = format_url_expiry(format.stream_url.as_ref());
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
        });
    }

    if let Some(hls_manifest_url) = hls_manifest_url.as_ref().filter(|url| !url.is_empty()) {
        return Ok(ChosenFormat {
            stream_url: hls_manifest_url.clone(),
            content_length: None,
            is_hls: true,
            headers: web_stream_headers(),
            range_chunk_size: None,
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

fn android_vr_stream_headers() -> Vec<PreparedHeader> {
    vec![PreparedHeader::new("User-Agent", ANDROID_VR_USER_AGENT)]
}

async fn fetch_android_vr_audio_format(
    probe_client: &Client,
    canonical_url: &str,
    video_id: &str,
) -> Result<Option<ChosenFormat>, ResolveError> {
    let Some(response) =
        fetch_android_vr_player_response(probe_client, canonical_url, video_id).await?
    else {
        return Ok(None);
    };
    Ok(response
        .streaming_data
        .as_ref()
        .and_then(chosen_format_from_android_streaming_data))
}

async fn fetch_android_vr_track_request_fast(
    raw_url: &str,
    probe_client: &Client,
) -> Result<Option<TrackRequest>, ResolveError> {
    let Some(video_id) = youtube_video_id_from_url(raw_url) else {
        return Ok(None);
    };

    match tokio::time::timeout(
        YOUTUBE_ANDROID_VR_FAST_PATH_TIMEOUT,
        fetch_android_vr_track_request(probe_client, raw_url, &video_id),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Ok(None),
    }
}

async fn fetch_android_vr_track_request(
    probe_client: &Client,
    raw_url: &str,
    video_id: &str,
) -> Result<Option<TrackRequest>, ResolveError> {
    let Some(response) = fetch_direct_android_vr_player_response(probe_client, video_id).await?
    else {
        return Ok(None);
    };
    let Some(details) = response.video_details else {
        return Ok(None);
    };
    let Some(streaming_data) = response.streaming_data else {
        return Ok(None);
    };
    let Some(format) = chosen_format_from_android_streaming_data(&streaming_data) else {
        return Ok(None);
    };

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

    Ok(Some(TrackRequest::new(
        "youtube",
        format!("youtube:video:{canonical_video_id}"),
        raw_url.to_owned(),
        canonical_url.clone(),
        canonical_url.clone(),
        prepared,
        TrackMetadata::new(title, author, canonical_url, thumbnail_url, duration),
    )))
}

async fn fetch_android_vr_player_response(
    probe_client: &Client,
    canonical_url: &str,
    video_id: &str,
) -> Result<Option<AndroidPlayerResponse>, ResolveError> {
    if let Some(response) = fetch_direct_android_vr_player_response(probe_client, video_id).await?
        && response.streaming_data.is_some()
    {
        return Ok(Some(response));
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
        return Ok(None);
    };
    let Some(visitor_data) = extract_visitor_data(&watch_html) else {
        return Ok(None);
    };

    probe_client
        .post("https://youtubei.googleapis.com/youtubei/v1/player?prettyPrint=false")
        .headers(android_vr_api_headers(visitor_data.as_str())?)
        .json(&android_vr_player_request(
            video_id,
            signature_timestamp,
            visitor_data.as_str(),
        ))
        .send()
        .await
        .map_err(ResolveError::Request)?
        .error_for_status()
        .map_err(ResolveError::Request)?
        .json::<AndroidPlayerResponse>()
        .await
        .map_err(ResolveError::Request)
        .map(Some)
}

async fn fetch_direct_android_vr_player_response(
    probe_client: &Client,
    video_id: &str,
) -> Result<Option<AndroidPlayerResponse>, ResolveError> {
    probe_client
        .post("https://youtubei.googleapis.com/youtubei/v1/player?prettyPrint=false")
        .headers(android_vr_direct_api_headers()?)
        .json(&direct_android_vr_player_request(video_id))
        .send()
        .await
        .map_err(ResolveError::Request)?
        .error_for_status()
        .map_err(ResolveError::Request)?
        .json::<AndroidPlayerResponse>()
        .await
        .map_err(ResolveError::Request)
        .map(Some)
}

fn chosen_format_from_android_streaming_data(
    streaming_data: &AndroidStreamingData,
) -> Option<ChosenFormat> {
    if let Some(format) = choose_android_audio_stream(&streaming_data.adaptive_formats) {
        let content_length = format
            .content_length
            .clone()
            .or_else(|| parse_content_length_from_url(format.url.as_str()).map(|v| v.to_string()));
        return Some(ChosenFormat {
            stream_url: format.url.clone(),
            content_length: content_length.clone(),
            is_hls: false,
            headers: android_vr_stream_headers(),
            range_chunk_size: content_length.is_some().then_some(YOUTUBE_RANGE_CHUNK_SIZE),
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
            headers: android_vr_stream_headers(),
            range_chunk_size: None,
        });
    }

    if let Some(server_abr_streaming_url) = streaming_data
        .server_abr_streaming_url
        .as_ref()
        .filter(|url| !url.is_empty())
    {
        return Some(ChosenFormat {
            stream_url: server_abr_streaming_url.clone(),
            content_length: None,
            is_hls: true,
            headers: android_vr_stream_headers(),
            range_chunk_size: None,
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
        .filter(|format| format.mime_type.starts_with("audio/") && !format.url.is_empty())
        .max_by_key(|format| {
            (
                format.mime_type.contains("opus"),
                format.audio_bitrate.or(format.bitrate).unwrap_or_default(),
                format.bitrate.unwrap_or_default(),
            )
        })
}

fn android_vr_api_headers(visitor_data: &str) -> Result<reqwest::header::HeaderMap, ResolveError> {
    use reqwest::header::{HeaderName, HeaderValue};

    let mut headers = android_vr_direct_api_headers()?;
    headers.insert(
        HeaderName::from_static("x-goog-visitor-id"),
        HeaderValue::from_str(visitor_data)
            .map_err(|_| ResolveError::InvalidHeaderValue(visitor_data.to_owned()))?,
    );

    Ok(headers)
}

fn android_vr_direct_api_headers() -> Result<reqwest::header::HeaderMap, ResolveError> {
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

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
        HeaderValue::from_str(ANDROID_VR_USER_AGENT)
            .map_err(|_| ResolveError::InvalidHeaderValue(ANDROID_VR_USER_AGENT.to_owned()))?,
    );

    Ok(headers)
}

fn direct_android_vr_player_request(video_id: &str) -> serde_json::Value {
    serde_json::json!({
        "context": {
            "client": {
                "clientName": "ANDROID_VR",
                "clientVersion": ANDROID_VR_CLIENT_VERSION,
                "userAgent": ANDROID_VR_USER_AGENT,
                "osName": "Android",
                "osVersion": "12L",
                "hl": "en",
                "timeZone": "UTC",
                "utcOffsetMinutes": 0,
                "androidSdkVersion": 32,
            }
        },
        "contentCheckOk": true,
        "racyCheckOk": true,
        "videoId": video_id,
    })
}

fn android_vr_player_request(
    video_id: &str,
    signature_timestamp: u64,
    visitor_data: &str,
) -> serde_json::Value {
    serde_json::json!({
        "context": {
            "client": {
                "clientName": "ANDROID_VR",
                "clientVersion": ANDROID_VR_CLIENT_VERSION,
                "userAgent": ANDROID_VR_USER_AGENT,
                "osName": "Android",
                "osVersion": "12L",
                "hl": "en",
                "timeZone": "UTC",
                "utcOffsetMinutes": 0,
                "androidSdkVersion": 32,
                "visitorData": visitor_data,
            }
        },
        "playbackContext": {
            "contentPlaybackContext": {
                "signatureTimestamp": signature_timestamp,
                "html5Preference": "HTML5_PREF_WANTS",
            }
        },
        "contentCheckOk": true,
        "racyCheckOk": true,
        "videoId": video_id,
    })
}

fn extract_signature_timestamp(html: &str) -> Option<u64> {
    let regex = Regex::new(r#""sts":(\d+)|"STS":(\d+)"#).ok()?;
    let captures = regex.captures(html)?;
    captures
        .get(1)
        .or_else(|| captures.get(2))
        .and_then(|capture| capture.as_str().parse::<u64>().ok())
}

fn extract_visitor_data(html: &str) -> Option<String> {
    let regex = Regex::new(r#""VISITOR_DATA":"([^"]+)""#).ok()?;
    regex
        .captures(html)?
        .get(1)
        .map(|capture| capture.as_str().to_owned())
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
}

#[derive(Clone, Debug, Deserialize)]
struct AndroidStreamingData {
    #[serde(rename = "adaptiveFormats", default)]
    adaptive_formats: Vec<AndroidAdaptiveFormat>,
    #[serde(rename = "hlsManifestUrl")]
    hls_manifest_url: Option<String>,
    #[serde(rename = "serverAbrStreamingUrl")]
    server_abr_streaming_url: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct AndroidAdaptiveFormat {
    #[serde(rename = "mimeType")]
    mime_type: String,
    url: String,
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
                url: "https://example.com/aac".to_owned(),
                bitrate: Some(128_000),
                audio_bitrate: Some(128),
                content_length: None,
            },
            AndroidAdaptiveFormat {
                mime_type: "audio/webm; codecs=\"opus\"".to_owned(),
                url: "https://example.com/opus".to_owned(),
                bitrate: Some(160_000),
                audio_bitrate: Some(160),
                content_length: None,
            },
        ];

        let chosen = choose_android_audio_stream(&formats).expect("android audio format");
        assert_eq!(chosen.url, "https://example.com/opus");
    }
}
