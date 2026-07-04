use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use reqwest::{Client, Url};
use serde_json::Value;
use wotoha_core::{PreparedSource, TrackMetadata, TrackRequest};

use crate::{ResolveError, provider::MediaProvider};

#[derive(Clone, Debug, Default)]
pub struct VimeoProvider;

#[derive(Clone, Debug, Eq, PartialEq)]
struct VimeoReference {
    video_id: String,
    unlisted_hash: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AudioMediaCandidate {
    uri: String,
    score: i32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProgressiveCandidate {
    url: String,
    area: u64,
}

#[async_trait]
impl MediaProvider for VimeoProvider {
    fn id(&self) -> &'static str {
        "vimeo"
    }

    fn supports(&self, raw_url: &str) -> bool {
        extract_vimeo_reference(raw_url).is_some()
    }

    async fn probe(
        &self,
        raw_url: &str,
        probe_client: &Client,
    ) -> Result<TrackRequest, ResolveError> {
        let reference = extract_vimeo_reference(raw_url)
            .ok_or_else(|| ResolveError::UnsupportedSource(raw_url.to_owned()))?;
        let config_url = reference.config_url();
        let payload: Value = probe_client
            .get(config_url)
            .send()
            .await
            .map_err(ResolveError::Request)?
            .error_for_status()
            .map_err(ResolveError::Request)?
            .json()
            .await
            .map_err(ResolveError::Request)?;

        track_request_from_config(raw_url, &payload, probe_client).await
    }
}

async fn track_request_from_config(
    raw_url: &str,
    payload: &Value,
    probe_client: &Client,
) -> Result<TrackRequest, ResolveError> {
    let video = payload
        .get("video")
        .ok_or_else(|| ResolveError::Parse("missing Vimeo video payload".to_owned()))?;
    let request = payload
        .get("request")
        .ok_or_else(|| ResolveError::Parse("missing Vimeo request payload".to_owned()))?;

    let video_id = video
        .get("id")
        .and_then(Value::as_u64)
        .ok_or_else(|| ResolveError::Parse("missing Vimeo video id".to_owned()))?;
    let canonical_url = video
        .get("share_url")
        .and_then(Value::as_str)
        .or_else(|| video.get("url").and_then(Value::as_str))
        .unwrap_or(raw_url);
    let expires_at_unix = request_expiry(request);

    let (prepared, source_url) = if let Some(master_url) = hls_master_url(request) {
        let audio_only_url = match fetch_audio_only_playlist(probe_client, &master_url).await {
            Ok(Some(url)) => url,
            Ok(None) => master_url.clone(),
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "failed to extract Vimeo audio-only playlist; falling back to master playlist"
                );
                master_url.clone()
            }
        };
        (
            PreparedSource::hls(audio_only_url.clone(), Vec::new(), expires_at_unix),
            audio_only_url,
        )
    } else {
        let stream_url = pick_progressive_stream(request)
            .ok_or_else(|| ResolveError::Parse("missing Vimeo playable source".to_owned()))?;
        (
            PreparedSource::http(stream_url.clone(), Vec::new(), None, expires_at_unix),
            stream_url,
        )
    };

    Ok(TrackRequest::new(
        "vimeo",
        format!("vimeo:video:{video_id}"),
        raw_url.to_owned(),
        canonical_url.to_owned(),
        source_url,
        prepared,
        TrackMetadata::new(
            video
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or(canonical_url)
                .to_owned(),
            video
                .get("owner")
                .and_then(|owner| owner.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("Vimeo")
                .to_owned(),
            canonical_url.to_owned(),
            video
                .get("thumbnail_url")
                .and_then(Value::as_str)
                .map(Arc::<str>::from),
            video
                .get("duration")
                .and_then(Value::as_u64)
                .map(Duration::from_secs),
        ),
    ))
}

fn extract_vimeo_reference(raw_url: &str) -> Option<VimeoReference> {
    let url = Url::parse(raw_url).ok()?;
    let host = url.host_str()?.to_ascii_lowercase();
    let segments: Vec<_> = url
        .path_segments()?
        .filter(|segment| !segment.is_empty())
        .collect();

    if host == "player.vimeo.com" {
        if segments.first().copied()? != "video" {
            return None;
        }

        let video_id = segments.get(1)?.to_string();
        if !video_id.chars().all(|ch| ch.is_ascii_digit()) {
            return None;
        }

        let unlisted_hash = url
            .query_pairs()
            .find(|(key, _)| key == "h")
            .map(|(_, value)| value.to_string());

        return Some(VimeoReference {
            video_id,
            unlisted_hash,
        });
    }

    if !matches!(host.as_str(), "vimeo.com" | "www.vimeo.com") {
        return None;
    }

    let video_index = segments
        .iter()
        .rposition(|segment| segment.chars().all(|ch| ch.is_ascii_digit()))?;
    let video_id = segments.get(video_index)?.to_string();
    let unlisted_hash = url
        .query_pairs()
        .find(|(key, _)| key == "h")
        .map(|(_, value)| value.to_string())
        .or_else(|| {
            segments
                .get(video_index + 1)
                .filter(|segment| looks_like_unlisted_hash(segment))
                .map(|segment| (*segment).to_string())
        });

    Some(VimeoReference {
        video_id,
        unlisted_hash,
    })
}

impl VimeoReference {
    fn config_url(&self) -> String {
        let mut url = format!("https://player.vimeo.com/video/{}/config", self.video_id);
        if let Some(hash) = &self.unlisted_hash {
            url.push_str("?h=");
            url.push_str(hash);
        }
        url
    }
}

fn looks_like_unlisted_hash(segment: &str) -> bool {
    segment.len() >= 6 && segment.chars().all(|ch| ch.is_ascii_hexdigit())
}

fn hls_master_url(request: &Value) -> Option<String> {
    let hls = request.get("files")?.get("hls")?;
    let default_cdn = hls.get("default_cdn")?.as_str()?;
    let cdn = hls.get("cdns")?.get(default_cdn)?;

    cdn.get("url")
        .and_then(Value::as_str)
        .or_else(|| cdn.get("avc_url").and_then(Value::as_str))
        .map(str::to_owned)
}

fn pick_progressive_stream(request: &Value) -> Option<String> {
    let progressive = request.get("files")?.get("progressive")?.as_array()?;
    let candidate = progressive
        .iter()
        .filter_map(|item| {
            Some(ProgressiveCandidate {
                url: item.get("url")?.as_str()?.to_owned(),
                area: item.get("width").and_then(Value::as_u64).unwrap_or(0)
                    * item.get("height").and_then(Value::as_u64).unwrap_or(0),
            })
        })
        .min_by_key(|candidate| candidate.area)?;

    Some(candidate.url)
}

fn request_expiry(request: &Value) -> Option<u64> {
    let timestamp = request.get("timestamp").and_then(Value::as_u64)?;
    let expires = request.get("expires").and_then(Value::as_u64)?;
    timestamp.checked_add(expires)
}

async fn fetch_audio_only_playlist(
    probe_client: &Client,
    master_url: &str,
) -> Result<Option<String>, ResolveError> {
    let manifest = probe_client
        .get(master_url)
        .send()
        .await
        .map_err(ResolveError::Request)?
        .error_for_status()
        .map_err(ResolveError::Request)?
        .text()
        .await
        .map_err(ResolveError::Request)?;

    Ok(extract_audio_only_playlist(master_url, &manifest))
}

fn extract_audio_only_playlist(master_url: &str, manifest: &str) -> Option<String> {
    let lines: Vec<_> = manifest
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();

    let mut best_media: Option<AudioMediaCandidate> = None;
    for line in &lines {
        if !line.starts_with("#EXT-X-MEDIA:") || !line.contains("TYPE=AUDIO") {
            continue;
        }

        let Some(uri) = attribute_value(line, "URI") else {
            continue;
        };
        let mut score = 0;
        if attribute_value(line, "DEFAULT").is_some_and(|value| value == "YES") {
            score += 1;
        }
        if attribute_value(line, "GROUP-ID")
            .is_some_and(|value| value.to_ascii_lowercase().contains("high"))
        {
            score += 2;
        }

        let candidate = AudioMediaCandidate {
            uri: uri.to_owned(),
            score,
        };
        let replace = match best_media.as_ref() {
            Some(current) => candidate.score >= current.score,
            None => true,
        };
        if replace {
            best_media = Some(candidate);
        }
    }

    if let Some(candidate) = best_media {
        return Url::parse(master_url)
            .ok()?
            .join(&candidate.uri)
            .ok()
            .map(|url| url.to_string());
    }

    None
}

fn attribute_value<'a>(line: &'a str, attribute: &str) -> Option<&'a str> {
    let marker = format!(r#"{attribute}=""#);
    let start = line.find(&marker)? + marker.len();
    let rest = &line[start..];
    let end = rest.find('"')?;
    Some(&rest[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supports_common_vimeo_url_shapes() {
        let provider = VimeoProvider;

        assert!(provider.supports("https://vimeo.com/76979871"));
        assert!(provider.supports("https://vimeo.com/channels/staffpicks/76979871"));
        assert!(provider.supports("https://player.vimeo.com/video/76979871?h=8272103f6e"));
        assert!(!provider.supports("https://example.com/76979871"));
    }

    #[test]
    fn extracts_unlisted_vimeo_hashes_from_share_urls() {
        let reference = extract_vimeo_reference("https://vimeo.com/148751763/1246a4d543").unwrap();
        assert_eq!(
            reference,
            VimeoReference {
                video_id: "148751763".to_owned(),
                unlisted_hash: Some("1246a4d543".to_owned()),
            }
        );
    }

    #[test]
    fn extracts_audio_only_vimeo_playlist() {
        let manifest = r#"
        #EXTM3U
        #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="audio-low",NAME="English",DEFAULT=YES,AUTOSELECT=YES,LANGUAGE="en",CHANNELS="2",URI="../../../ccfaa6de/avf/a565ff5d/media.m3u8?st=audio-low"
        #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="audio-high",NAME="English",DEFAULT=YES,AUTOSELECT=YES,LANGUAGE="en",CHANNELS="2",URI="../../../ccfaa6de/avf/5850de2a/media.m3u8?st=audio-high"
        #EXT-X-STREAM-INF:BANDWIDTH=3063040,AUDIO="audio-high"
        ../../../ccfaa6de/avf/5850de2a/media.m3u8?st=video
        "#;

        let audio_only = extract_audio_only_playlist(
            "https://vod-adaptive-ak.vimeocdn.com/path/to/master.m3u8",
            manifest,
        )
        .unwrap();

        assert_eq!(
            audio_only,
            "https://vod-adaptive-ak.vimeocdn.com/ccfaa6de/avf/5850de2a/media.m3u8?st=audio-high"
        );
    }
}
