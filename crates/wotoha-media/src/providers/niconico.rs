use std::{collections::HashMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use reqwest::{
    Client, Url,
    header::{COOKIE, HeaderMap, HeaderValue},
};
use serde_json::{Value, json};
use wotoha_core::{PreparedHeader, PreparedSource, TrackMetadata, TrackRequest};

use crate::{
    ResolveError,
    html::{decode_html_attribute, extract_meta_content},
    provider::MediaProvider,
};

const NICONICO_FRONTEND_ID: &str = "6";
const NICONICO_FRONTEND_VERSION: &str = "0";
const NICONICO_REQUEST_WITH: &str = "https://www.nicovideo.jp";
const NICONICO_MAX_DOMAND_VIDEO_OUTPUTS: usize = 2;
const NICONICO_MAX_DOMAND_AUDIO_OUTPUTS: usize = 3;

#[derive(Clone, Debug, Default)]
pub struct NiconicoProvider;

#[async_trait]
impl MediaProvider for NiconicoProvider {
    fn id(&self) -> &'static str {
        "niconico"
    }

    fn supports(&self, raw_url: &str) -> bool {
        let Ok(url) = Url::parse(raw_url) else {
            return false;
        };

        matches!(
            url.host_str().map(|host| host.to_ascii_lowercase()),
            Some(host)
                if matches!(host.as_str(), "nicovideo.jp" | "www.nicovideo.jp" | "nico.ms")
        )
    }

    async fn probe(
        &self,
        raw_url: &str,
        probe_client: &Client,
    ) -> Result<TrackRequest, ResolveError> {
        let response = probe_client
            .get(raw_url)
            .send()
            .await
            .map_err(ResolveError::Request)?
            .error_for_status()
            .map_err(ResolveError::Request)?;
        let mut playback_cookies = HashMap::new();
        collect_response_cookies(&response, &mut playback_cookies);
        let page = response.text().await.map_err(ResolveError::Request)?;

        let payload = parse_server_response(&page)?;
        let response = payload
            .get("data")
            .and_then(|value| value.get("response"))
            .ok_or_else(|| ResolveError::Parse("missing NicoNico response payload".to_owned()))?;

        let video = response
            .get("video")
            .ok_or_else(|| ResolveError::Parse("missing NicoNico video payload".to_owned()))?;
        let client = response
            .get("client")
            .ok_or_else(|| ResolveError::Parse("missing NicoNico client payload".to_owned()))?;
        let domand = response
            .get("media")
            .and_then(|value| value.get("domand"))
            .ok_or_else(|| ResolveError::Parse("missing NicoNico DOMAND payload".to_owned()))?;

        let watch_id = video
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| ResolveError::Parse("missing NicoNico watch id".to_owned()))?;
        let watch_track_id = client
            .get("watchTrackId")
            .and_then(Value::as_str)
            .ok_or_else(|| ResolveError::Parse("missing NicoNico watchTrackId".to_owned()))?;
        let access_right_key = domand
            .get("accessRightKey")
            .and_then(Value::as_str)
            .ok_or_else(|| ResolveError::Parse("missing NicoNico accessRightKey".to_owned()))?;

        let outputs = collect_domand_outputs(domand)?;
        let playback = request_domand_playlist(
            probe_client,
            watch_id,
            watch_track_id,
            access_right_key,
            &outputs,
            &mut playback_cookies,
        )
        .await?;

        let canonical_url = format!("https://www.nicovideo.jp/watch/{watch_id}");
        let prepared_headers = build_prepared_headers(&playback_cookies);
        let playback_source_url = if looks_like_hls(&playback.content_url) {
            resolve_audio_playlist_url(probe_client, &playback.content_url, &prepared_headers)
                .await?
        } else {
            playback.content_url.clone()
        };
        let prepared = if looks_like_hls(&playback_source_url) {
            PreparedSource::hls(playback_source_url.clone(), prepared_headers, None)
        } else {
            PreparedSource::http(playback_source_url.clone(), prepared_headers, None, None)
        };

        Ok(TrackRequest::new(
            self.id(),
            format!("niconico:video:{watch_id}"),
            raw_url.to_owned(),
            canonical_url.clone(),
            playback_source_url,
            prepared,
            TrackMetadata::new(
                video
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or(watch_id)
                    .to_owned(),
                response
                    .get("owner")
                    .and_then(|value| value.get("nickname"))
                    .and_then(Value::as_str)
                    .unwrap_or("niconico")
                    .to_owned(),
                canonical_url,
                video
                    .get("thumbnail")
                    .and_then(|value| {
                        value
                            .get("ogp")
                            .and_then(Value::as_str)
                            .or_else(|| value.get("player").and_then(Value::as_str))
                            .or_else(|| value.get("url").and_then(Value::as_str))
                    })
                    .map(Arc::<str>::from),
                video
                    .get("duration")
                    .and_then(Value::as_u64)
                    .map(Duration::from_secs),
            ),
        ))
    }
}

fn parse_server_response(page: &str) -> Result<Value, ResolveError> {
    let raw = extract_meta_content(page, "name", "server-response")
        .ok_or_else(|| ResolveError::Parse("missing NicoNico server-response meta".to_owned()))?;
    serde_json::from_str(&decode_html_attribute(&raw))
        .map_err(|error| ResolveError::Parse(error.to_string()))
}

fn collect_domand_outputs(domand: &Value) -> Result<Vec<[String; 2]>, ResolveError> {
    let videos = domand
        .get("videos")
        .and_then(Value::as_array)
        .ok_or_else(|| ResolveError::Parse("missing NicoNico DOMAND video qualities".to_owned()))?;
    let audios = domand
        .get("audios")
        .and_then(Value::as_array)
        .ok_or_else(|| ResolveError::Parse("missing NicoNico DOMAND audio qualities".to_owned()))?;

    let video_ids = available_quality_ids(videos);
    let audio_ids = available_quality_ids(audios);
    if video_ids.is_empty() || audio_ids.is_empty() {
        return Err(ResolveError::Parse(
            "NicoNico DOMAND did not expose a playable quality pair".to_owned(),
        ));
    }

    let mut outputs = Vec::with_capacity(video_ids.len() * audio_ids.len());
    for video_id in video_ids.iter().take(NICONICO_MAX_DOMAND_VIDEO_OUTPUTS) {
        for audio_id in audio_ids.iter().take(NICONICO_MAX_DOMAND_AUDIO_OUTPUTS) {
            outputs.push([video_id.clone(), audio_id.clone()]);
        }
    }

    Ok(outputs)
}

fn available_quality_ids(items: &[Value]) -> Vec<String> {
    let mut pairs: Vec<(i64, String)> = items
        .iter()
        .filter(|item| {
            item.get("isAvailable")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .filter_map(|item| {
            Some((
                item.get("qualityLevel")
                    .and_then(Value::as_i64)
                    .unwrap_or_default(),
                item.get("id")?.as_str()?.to_owned(),
            ))
        })
        .collect();
    pairs.sort_by_key(|pair| std::cmp::Reverse(pair.0));
    pairs.into_iter().map(|(_, id)| id).collect()
}

async fn request_domand_playlist(
    probe_client: &Client,
    watch_id: &str,
    watch_track_id: &str,
    access_right_key: &str,
    outputs: &[[String; 2]],
    playback_cookies: &mut HashMap<String, String>,
) -> Result<NiconicoPlayback, ResolveError> {
    let uri = format!(
        "https://nvapi.nicovideo.jp/v1/watch/{watch_id}/access-rights/hls?actionTrackId={watch_track_id}"
    );
    let body = json!({ "outputs": outputs });

    let mut headers = HeaderMap::new();
    headers.insert(
        "X-Frontend-Id",
        NICONICO_FRONTEND_ID
            .parse()
            .expect("frontend id should be a valid header"),
    );
    headers.insert(
        "X-Frontend-Version",
        NICONICO_FRONTEND_VERSION
            .parse()
            .expect("frontend version should be a valid header"),
    );
    headers.insert(
        "X-Request-With",
        NICONICO_REQUEST_WITH
            .parse()
            .expect("request-with should be a valid header"),
    );
    headers.insert(
        "X-Access-Right-Key",
        access_right_key.parse().map_err(|_| {
            ResolveError::InvalidHeaderValue("invalid NicoNico access right key".to_owned())
        })?,
    );

    let response = probe_client
        .post(uri)
        .headers(headers)
        .json(&body)
        .send()
        .await
        .map_err(ResolveError::Request)?
        .error_for_status()
        .map_err(ResolveError::Request)?;
    collect_response_cookies(&response, playback_cookies);
    let payload: Value = response.json().await.map_err(ResolveError::Request)?;

    let content_url = payload
        .get("data")
        .and_then(|value| value.get("contentUrl"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| ResolveError::Parse("missing NicoNico DOMAND contentUrl".to_owned()))?;

    Ok(NiconicoPlayback { content_url })
}

fn looks_like_hls(stream_url: &str) -> bool {
    stream_url.contains(".m3u8")
}

struct NiconicoPlayback {
    content_url: String,
}

fn collect_response_cookies(
    response: &reqwest::Response,
    playback_cookies: &mut HashMap<String, String>,
) {
    for cookie in response.cookies() {
        playback_cookies.insert(cookie.name().to_owned(), cookie.value().to_owned());
    }
}

fn build_prepared_headers(playback_cookies: &HashMap<String, String>) -> Vec<PreparedHeader> {
    if playback_cookies.is_empty() {
        return Vec::new();
    }

    let mut cookies: Vec<(&str, &str)> = playback_cookies
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    cookies.sort_by(|left, right| left.0.cmp(right.0));

    let cookie_header = cookies
        .into_iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("; ");
    vec![PreparedHeader::new("Cookie", cookie_header)]
}

async fn resolve_audio_playlist_url(
    probe_client: &Client,
    master_playlist_url: &str,
    prepared_headers: &[PreparedHeader],
) -> Result<String, ResolveError> {
    let mut request = probe_client.get(master_playlist_url);
    if let Some(cookie_header) = prepared_headers
        .iter()
        .find(|header| header.name.as_ref().eq_ignore_ascii_case("cookie"))
    {
        let cookie = HeaderValue::from_str(cookie_header.value.as_ref()).map_err(|_| {
            ResolveError::InvalidHeaderValue("invalid NicoNico cookie header".to_owned())
        })?;
        request = request.header(COOKIE, cookie);
    }

    let playlist = request
        .send()
        .await
        .map_err(ResolveError::Request)?
        .error_for_status()
        .map_err(ResolveError::Request)?
        .text()
        .await
        .map_err(ResolveError::Request)?;

    Ok(select_best_audio_playlist(master_playlist_url, &playlist)
        .unwrap_or_else(|| master_playlist_url.to_owned()))
}

fn select_best_audio_playlist(master_playlist_url: &str, playlist: &str) -> Option<String> {
    playlist
        .lines()
        .filter(|line| line.starts_with("#EXT-X-MEDIA:TYPE=AUDIO"))
        .filter_map(|line| {
            let uri = extract_attribute(line, "URI")?;
            let bitrate = line
                .split("URI=\"")
                .nth(1)
                .and_then(|rest| rest.split('"').next())
                .map(audio_uri_bitrate)
                .unwrap_or_default();
            Some((bitrate, uri.to_owned()))
        })
        .max_by_key(|(bitrate, _)| *bitrate)
        .and_then(|(_, uri)| resolve_playlist_uri(master_playlist_url, &uri))
}

fn resolve_playlist_uri(master_playlist_url: &str, uri: &str) -> Option<String> {
    Url::parse(master_playlist_url)
        .ok()?
        .join(uri)
        .ok()
        .map(|url| url.to_string())
}

fn extract_attribute<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let pattern = format!("{name}=\"");
    let start = line.find(&pattern)? + pattern.len();
    let tail = &line[start..];
    let end = tail.find('"')?;
    Some(&tail[..end])
}

fn audio_uri_bitrate(uri: &str) -> u32 {
    let marker = "audio-aac-";
    let Some(start) = uri.find(marker).map(|index| index + marker.len()) else {
        return 0;
    };
    let tail = &uri[start..];
    let digits: String = tail.chars().take_while(|ch| ch.is_ascii_digit()).collect();
    digits.parse().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_all_domand_output_pairs_in_quality_order() {
        let domand = json!({
            "videos": [
                { "id": "video-low", "isAvailable": true, "qualityLevel": 0 },
                { "id": "video-high", "isAvailable": true, "qualityLevel": 2 }
            ],
            "audios": [
                { "id": "audio-low", "isAvailable": true, "qualityLevel": 0 },
                { "id": "audio-high", "isAvailable": true, "qualityLevel": 1 }
            ]
        });

        let outputs = collect_domand_outputs(&domand).unwrap();
        assert_eq!(
            outputs,
            vec![
                ["video-high".to_owned(), "audio-high".to_owned()],
                ["video-high".to_owned(), "audio-low".to_owned()],
                ["video-low".to_owned(), "audio-high".to_owned()],
                ["video-low".to_owned(), "audio-low".to_owned()],
            ]
        );
    }

    #[test]
    fn builds_cookie_header_from_playback_cookies() {
        let headers = build_prepared_headers(&HashMap::from([
            ("domand_bid".to_owned(), "abc".to_owned()),
            ("nicosid".to_owned(), "xyz".to_owned()),
        ]));

        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].name.as_ref(), "Cookie");
        assert_eq!(headers[0].value.as_ref(), "domand_bid=abc; nicosid=xyz");
    }

    #[test]
    fn selects_highest_bitrate_audio_playlist_from_master_playlist() {
        let playlist = "#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio-aac-64kbps\",URI=\"https://example.com/audio-aac-64kbps.m3u8\"\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio-aac-192kbps\",URI=\"https://example.com/audio-aac-192kbps.m3u8\"\n";
        assert_eq!(
            select_best_audio_playlist("https://example.com/master.m3u8", playlist).as_deref(),
            Some("https://example.com/audio-aac-192kbps.m3u8")
        );
    }

    #[test]
    fn resolves_relative_audio_playlist_from_master_playlist() {
        let playlist = "#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio-aac-192kbps\",URI=\"audio/audio-aac-192kbps.m3u8\"\n";

        assert_eq!(
            select_best_audio_playlist("https://example.com/path/master.m3u8", playlist).as_deref(),
            Some("https://example.com/path/audio/audio-aac-192kbps.m3u8")
        );
    }
}
