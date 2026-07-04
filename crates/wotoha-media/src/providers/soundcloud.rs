use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use regex::Regex;
use reqwest::{Client, StatusCode, Url};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tokio::sync::{Mutex, RwLock};
use wotoha_core::{PreparedSource, TrackMetadata, TrackRequest};

use crate::{ResolveError, provider::MediaProvider};

const SOUNDCLOUD_HOME_URL: &str = "https://soundcloud.com";
const SOUNDCLOUD_RESOLVE_URL: &str = "https://api-v2.soundcloud.com/resolve";
const SOUNDCLOUD_TRACKS_URL: &str = "https://api-v2.soundcloud.com/tracks";

#[derive(Clone, Debug, Default)]
pub struct SoundCloudProvider {
    client_id: Arc<RwLock<Option<String>>>,
    client_id_refresh: Arc<Mutex<()>>,
}

#[derive(Debug, Deserialize)]
struct HydrationEntry {
    hydratable: String,
    data: serde_json::Value,
}

#[derive(Clone, Debug, Deserialize)]
struct SoundHydration {
    id: u64,
    urn: Option<String>,
    title: Option<String>,
    permalink_url: Option<String>,
    artwork_url: Option<String>,
    duration: Option<u64>,
    user: Option<SoundUser>,
    media: Option<SoundMedia>,
}

#[derive(Clone, Debug, Deserialize)]
struct SoundUser {
    username: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct SoundMedia {
    transcodings: Vec<SoundTranscoding>,
}

#[derive(Clone, Debug, Deserialize)]
struct SoundTranscoding {
    url: String,
    preset: Option<String>,
    quality: Option<String>,
    format: SoundFormat,
}

#[derive(Clone, Debug, Deserialize)]
struct SoundFormat {
    protocol: String,
    mime_type: String,
}

#[derive(Debug, Deserialize)]
struct StreamResponse {
    url: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SoundCloudProbeAction {
    FallbackToHtml,
    Fail,
}

#[async_trait]
impl MediaProvider for SoundCloudProvider {
    fn id(&self) -> &'static str {
        "soundcloud"
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
                    "soundcloud.com"
                        | "www.soundcloud.com"
                        | "m.soundcloud.com"
                        | "on.soundcloud.com"
                )
        )
    }

    async fn probe(
        &self,
        raw_url: &str,
        probe_client: &Client,
    ) -> Result<TrackRequest, ResolveError> {
        match self.resolve_track_from_url(probe_client, raw_url).await {
            Ok(track) => {
                self.track_request_from_track(raw_url, raw_url, &track, probe_client)
                    .await
            }
            Err(error) => match classify_probe_error(&error) {
                SoundCloudProbeAction::Fail => Err(error),
                SoundCloudProbeAction::FallbackToHtml => {
                    let response = probe_client
                        .get(raw_url)
                        .header("Accept-Language", "en-US,en;q=0.9")
                        .send()
                        .await
                        .map_err(ResolveError::Request)?
                        .error_for_status()
                        .map_err(ResolveError::Request)?;
                    let final_url = response.url().to_string();
                    let page = response.text().await.map_err(ResolveError::Request)?;
                    let track = parse_sound_hydration(&page)?;
                    self.track_request_from_track(raw_url, &final_url, &track, probe_client)
                        .await
                }
            },
        }
    }

    async fn refresh_playback(
        &self,
        request: &TrackRequest,
        probe_client: &Client,
    ) -> Result<Option<TrackRequest>, ResolveError> {
        let Some(track_id) = soundcloud_track_id(request) else {
            return Ok(None);
        };
        let track = self.fetch_track(probe_client, track_id).await?;
        self.track_request_from_track(
            request.requested_url.as_ref(),
            request.canonical_url.as_ref(),
            &track,
            probe_client,
        )
        .await
        .map(Some)
    }
}

impl SoundCloudProvider {
    async fn resolve_track_from_url(
        &self,
        probe_client: &Client,
        raw_url: &str,
    ) -> Result<SoundHydration, ResolveError> {
        let endpoint = Url::parse_with_params(SOUNDCLOUD_RESOLVE_URL, [("url", raw_url)])
            .map_err(|error| ResolveError::Parse(error.to_string()))?;
        self.api_get_with_client_id(probe_client, endpoint.as_ref())
            .await
    }

    async fn fetch_track(
        &self,
        probe_client: &Client,
        track_id: u64,
    ) -> Result<SoundHydration, ResolveError> {
        self.api_get_with_client_id(probe_client, &format!("{SOUNDCLOUD_TRACKS_URL}/{track_id}"))
            .await
    }

    async fn track_request_from_track(
        &self,
        requested_url: &str,
        fallback_url: &str,
        track: &SoundHydration,
        probe_client: &Client,
    ) -> Result<TrackRequest, ResolveError> {
        let (stream_url, transcoding) = self.resolve_stream_url(probe_client, track).await?;
        let prepared = if transcoding.format.protocol == "hls" || stream_url.contains(".m3u8") {
            PreparedSource::hls(stream_url.clone(), Vec::new(), None)
        } else {
            PreparedSource::http(stream_url.clone(), Vec::new(), None, None)
        };
        let canonical_url = track
            .permalink_url
            .clone()
            .unwrap_or_else(|| fallback_url.to_owned());
        let canonical_key = track
            .urn
            .clone()
            .unwrap_or_else(|| format!("soundcloud:tracks:{}", track.id));

        Ok(TrackRequest::new(
            "soundcloud",
            canonical_key,
            requested_url.to_owned(),
            canonical_url.clone(),
            stream_url,
            prepared,
            TrackMetadata::new(
                track.title.clone().unwrap_or_else(|| canonical_url.clone()),
                track
                    .user
                    .as_ref()
                    .and_then(|user| user.username.clone())
                    .unwrap_or_else(|| "SoundCloud".to_owned()),
                canonical_url,
                track
                    .artwork_url
                    .as_ref()
                    .map(|url| Arc::<str>::from(url.clone())),
                track.duration.map(Duration::from_millis),
            ),
        ))
    }

    async fn resolve_stream_url(
        &self,
        probe_client: &Client,
        track: &SoundHydration,
    ) -> Result<(String, SoundTranscoding), ResolveError> {
        let mut transcodings = track
            .media
            .as_ref()
            .ok_or_else(|| ResolveError::Parse("missing SoundCloud media payload".to_owned()))?
            .transcodings
            .iter()
            .filter(|transcoding| is_supported_transcoding(transcoding))
            .cloned()
            .collect::<Vec<_>>();
        transcodings.sort_by_key(score_transcoding);
        transcodings.reverse();

        let mut last_error = None;
        for transcoding in transcodings {
            match self
                .api_get_with_client_id::<StreamResponse>(probe_client, &transcoding.url)
                .await
            {
                Ok(stream) => {
                    let Some(url) = stream.url else {
                        last_error = Some(ResolveError::Parse(
                            "missing SoundCloud resolved stream URL".to_owned(),
                        ));
                        continue;
                    };

                    return Ok((url, transcoding));
                }
                Err(error) => {
                    last_error = Some(error);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| {
            ResolveError::Parse("missing SoundCloud playable transcoding".to_owned())
        }))
    }

    async fn api_get_with_client_id<T>(
        &self,
        probe_client: &Client,
        endpoint: &str,
    ) -> Result<T, ResolveError>
    where
        T: DeserializeOwned,
    {
        let mut refreshed = false;

        loop {
            let client_id = if refreshed {
                self.refresh_client_id(probe_client).await?
            } else {
                self.cached_client_id(probe_client).await?
            };

            let response = probe_client
                .get(endpoint)
                .query(&[("client_id", client_id.as_str())])
                .header("Accept", "application/json")
                .header("Referer", SOUNDCLOUD_HOME_URL)
                .send()
                .await
                .map_err(ResolveError::Request)?;

            if response.status() == StatusCode::UNAUTHORIZED && !refreshed {
                refreshed = true;
                continue;
            }

            let response = response.error_for_status().map_err(ResolveError::Request)?;
            return response.json::<T>().await.map_err(ResolveError::Request);
        }
    }

    async fn cached_client_id(&self, probe_client: &Client) -> Result<String, ResolveError> {
        if let Some(client_id) = self.client_id.read().await.clone() {
            return Ok(client_id);
        }

        let _refresh = self.client_id_refresh.lock().await;
        if let Some(client_id) = self.client_id.read().await.clone() {
            return Ok(client_id);
        }

        self.refresh_client_id_unlocked(probe_client).await
    }

    async fn refresh_client_id(&self, probe_client: &Client) -> Result<String, ResolveError> {
        let _refresh = self.client_id_refresh.lock().await;
        self.refresh_client_id_unlocked(probe_client).await
    }

    async fn refresh_client_id_unlocked(
        &self,
        probe_client: &Client,
    ) -> Result<String, ResolveError> {
        let client_id = fetch_client_id(probe_client).await?;
        *self.client_id.write().await = Some(client_id.clone());
        Ok(client_id)
    }
}

fn parse_sound_hydration(page: &str) -> Result<SoundHydration, ResolveError> {
    let raw = extract_hydration_json(page)
        .ok_or_else(|| ResolveError::Parse("missing SoundCloud hydration payload".to_owned()))?;
    let entries: Vec<HydrationEntry> =
        serde_json::from_str(raw).map_err(|error| ResolveError::Parse(error.to_string()))?;

    entries
        .into_iter()
        .find(|entry| entry.hydratable == "sound")
        .ok_or_else(|| ResolveError::Parse("missing SoundCloud sound hydration".to_owned()))
        .and_then(|entry| {
            serde_json::from_value::<SoundHydration>(entry.data)
                .map_err(|error| ResolveError::Parse(error.to_string()))
        })
}

fn extract_hydration_json(page: &str) -> Option<&str> {
    let marker = "window.__sc_hydration = ";
    let start = page.find(marker)? + marker.len();
    let tail = &page[start..];
    let end = tail.find(";</script>")?;
    Some(tail[..end].trim())
}

async fn fetch_client_id(probe_client: &Client) -> Result<String, ResolveError> {
    let home = probe_client
        .get(SOUNDCLOUD_HOME_URL)
        .header("Accept-Language", "en-US,en;q=0.9")
        .send()
        .await
        .map_err(ResolveError::Request)?
        .error_for_status()
        .map_err(ResolveError::Request)?
        .text()
        .await
        .map_err(ResolveError::Request)?;

    for script_url in extract_script_urls(&home) {
        let Ok(response) = probe_client
            .get(&script_url)
            .header("Referer", SOUNDCLOUD_HOME_URL)
            .send()
            .await
        else {
            continue;
        };
        let Ok(response) = response.error_for_status() else {
            continue;
        };
        let Ok(js) = response.text().await else {
            continue;
        };

        if let Some(client_id) = extract_client_id(&js) {
            return Ok(client_id);
        }
    }

    Err(ResolveError::Parse(
        "missing SoundCloud client_id in web assets".to_owned(),
    ))
}

fn extract_script_urls(page: &str) -> Vec<String> {
    script_url_regex()
        .captures_iter(page)
        .filter_map(|capture| capture.get(1))
        .map(|capture| capture.as_str().to_owned())
        .collect()
}

fn extract_client_id(js: &str) -> Option<String> {
    client_id_regex()
        .captures(js)
        .and_then(|capture| capture.get(1))
        .map(|capture| capture.as_str().to_owned())
}

fn classify_probe_error(error: &ResolveError) -> SoundCloudProbeAction {
    match error {
        ResolveError::Parse(_) => SoundCloudProbeAction::FallbackToHtml,
        ResolveError::Request(request_error) => classify_probe_status(request_error.status()),
        _ => SoundCloudProbeAction::Fail,
    }
}

fn classify_probe_status(status: Option<StatusCode>) -> SoundCloudProbeAction {
    match status {
        Some(StatusCode::BAD_REQUEST)
        | Some(StatusCode::NOT_FOUND)
        | Some(StatusCode::GONE)
        | Some(StatusCode::UNPROCESSABLE_ENTITY) => SoundCloudProbeAction::FallbackToHtml,
        _ => SoundCloudProbeAction::Fail,
    }
}

fn score_transcoding(transcoding: &SoundTranscoding) -> (u8, u8, u8) {
    let protocol_score = match transcoding.format.protocol.as_str() {
        // Prefer the complete MP3 stream. SoundCloud's HLS playlists introduce
        // roughly ten-second fragment boundaries and extra requests into playback.
        "progressive" => 2,
        "hls" => 1,
        _ => 0,
    };
    let mime_score = if transcoding.format.mime_type.contains("audio/mpeg")
        && !transcoding.format.mime_type.contains("mpegurl")
    {
        2
    } else if transcoding.format.mime_type.contains("mpegurl") {
        1
    } else {
        0
    };
    let quality_score = if transcoding.quality.as_deref() == Some("sq") {
        1
    } else {
        0
    };
    let preset_score = if transcoding
        .preset
        .as_deref()
        .is_some_and(|preset| preset.contains("abr"))
    {
        1
    } else {
        0
    };

    (protocol_score, mime_score, quality_score + preset_score)
}

fn is_supported_transcoding(transcoding: &SoundTranscoding) -> bool {
    let is_mp3 = transcoding.format.mime_type.contains("audio/mpeg");
    match transcoding.format.protocol.as_str() {
        "progressive" | "hls" => is_mp3,
        _ => false,
    }
}

fn soundcloud_track_id(request: &TrackRequest) -> Option<u64> {
    request
        .canonical_key
        .strip_prefix("soundcloud:tracks:")
        .and_then(|value| value.parse().ok())
}

fn script_url_regex() -> &'static Regex {
    static SCRIPT_URL_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    SCRIPT_URL_RE.get_or_init(|| {
        Regex::new(r#"<script[^>]+src="([^"]+\.js[^"]*)""#)
            .expect("SoundCloud script URL regex should compile")
    })
}

fn client_id_regex() -> &'static Regex {
    static CLIENT_ID_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    CLIENT_ID_RE.get_or_init(|| {
        Regex::new(r#"client_id[:=]"?(\w{32})"#).expect("SoundCloud client_id regex should compile")
    })
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };

    use super::*;
    use wotoha_core::{PreparedSource, TrackMetadata, TrackRequest};

    const TEST_CLIENT_ID: &str = "ceeWbO4nf8MvuTeipNw0E3Lkh3NNxzMy";

    #[test]
    fn extracts_hydration_json_payload() {
        let page = r#"
        <script>window.__sc_hydration = [{"hydratable":"sound","data":{"id":293,"title":"Flickermood"}}];</script>
        "#;

        assert_eq!(
            extract_hydration_json(page),
            Some(r#"[{"hydratable":"sound","data":{"id":293,"title":"Flickermood"}}]"#)
        );
    }

    #[test]
    fn parses_soundcloud_client_id_from_asset() {
        let js = r#"a="client_id=ceeWbO4nf8MvuTeipNw0E3Lkh3NNxzMy";"#;
        assert_eq!(
            extract_client_id(js),
            Some("ceeWbO4nf8MvuTeipNw0E3Lkh3NNxzMy".to_owned())
        );
    }

    #[test]
    fn scores_progressive_soundcloud_transcoding_above_hls() {
        let track = sample_track("https://api-v2.soundcloud.com/media");

        assert_eq!(
            track
                .media
                .as_ref()
                .unwrap()
                .transcodings
                .iter()
                .max_by_key(|transcoding| score_transcoding(transcoding))
                .map(|transcoding| transcoding.url.as_str()),
            Some("https://api-v2.soundcloud.com/media/progressive")
        );
    }

    #[test]
    fn excludes_fragmented_mp4_hls_without_initialization_map_support() {
        let transcoding = SoundTranscoding {
            url: "https://api-v2.soundcloud.com/media/aac-hls".to_owned(),
            preset: Some("aac_160k".to_owned()),
            quality: Some("hq".to_owned()),
            format: SoundFormat {
                protocol: "hls".to_owned(),
                mime_type: "audio/mp4; codecs=\"mp4a.40.2\"".to_owned(),
            },
        };

        assert!(!is_supported_transcoding(&transcoding));
    }

    #[tokio::test]
    async fn prepares_progressive_stream_as_http() {
        let (base_url, server) = spawn_stream_api([(
            "/progressive",
            200,
            r#"{"url":"https://cf-media.sndcdn.com/full-track.mp3"}"#,
        )]);
        let provider = provider_with_cached_client_id().await;

        let request = provider
            .track_request_from_track(
                "https://soundcloud.com/forss/flickermood",
                "https://soundcloud.com/forss/flickermood",
                &sample_track(&base_url),
                &Client::new(),
            )
            .await
            .unwrap();

        assert!(matches!(
            request.prepared,
            PreparedSource::Http { ref stream_url, .. }
                if stream_url.as_ref() == "https://cf-media.sndcdn.com/full-track.mp3"
        ));
        assert_eq!(server.join().unwrap(), vec!["/progressive"]);
    }

    #[tokio::test]
    async fn falls_back_to_hls_when_progressive_resolution_fails() {
        let (base_url, server) = spawn_stream_api([
            ("/progressive", 404, ""),
            (
                "/hls",
                200,
                r#"{"url":"https://cf-hls-media.sndcdn.com/full-track/playlist.m3u8"}"#,
            ),
        ]);
        let provider = provider_with_cached_client_id().await;

        let request = provider
            .track_request_from_track(
                "https://soundcloud.com/forss/flickermood",
                "https://soundcloud.com/forss/flickermood",
                &sample_track(&base_url),
                &Client::new(),
            )
            .await
            .unwrap();

        assert!(matches!(
            request.prepared,
            PreparedSource::Hls { ref playlist_url, .. }
                if playlist_url.as_ref()
                    == "https://cf-hls-media.sndcdn.com/full-track/playlist.m3u8"
        ));
        assert_eq!(server.join().unwrap(), vec!["/progressive", "/hls"]);
    }

    fn sample_track(media_base_url: &str) -> SoundHydration {
        SoundHydration {
            id: 293,
            urn: None,
            title: Some("Flickermood".to_owned()),
            permalink_url: None,
            artwork_url: None,
            duration: Some(213886),
            user: None,
            media: Some(SoundMedia {
                transcodings: vec![
                    SoundTranscoding {
                        url: format!("{media_base_url}/progressive"),
                        preset: Some("mp3_0_0".to_owned()),
                        quality: Some("sq".to_owned()),
                        format: SoundFormat {
                            protocol: "progressive".to_owned(),
                            mime_type: "audio/mpeg".to_owned(),
                        },
                    },
                    SoundTranscoding {
                        url: format!("{media_base_url}/hls"),
                        preset: Some("abr_sq".to_owned()),
                        quality: Some("sq".to_owned()),
                        format: SoundFormat {
                            protocol: "hls".to_owned(),
                            mime_type: "audio/mpegurl".to_owned(),
                        },
                    },
                ],
            }),
        }
    }

    async fn provider_with_cached_client_id() -> SoundCloudProvider {
        let provider = SoundCloudProvider::default();
        *provider.client_id.write().await = Some(TEST_CLIENT_ID.to_owned());
        provider
    }

    fn spawn_stream_api<const N: usize>(
        responses: [(&'static str, u16, &'static str); N],
    ) -> (String, thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let mut paths = Vec::with_capacity(N);
            for (expected_path, status, body) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut buffer = [0_u8; 1024];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let read = stream.read(&mut buffer).unwrap();
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                }
                let request_line = String::from_utf8_lossy(&request);
                let path = request_line
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .and_then(|target| target.split('?').next())
                    .unwrap_or_default()
                    .to_owned();
                assert_eq!(path, expected_path);
                paths.push(path);

                let reason = if status == 200 { "OK" } else { "Not Found" };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
            }
            paths
        });
        (base_url, server)
    }

    #[test]
    fn extracts_track_id_from_canonical_key() {
        let request = TrackRequest::new(
            "soundcloud",
            "soundcloud:tracks:293",
            "https://soundcloud.com/forss/flickermood",
            "https://soundcloud.com/forss/flickermood",
            "https://cf-hls-media.sndcdn.com/media/playlist.m3u8",
            PreparedSource::hls(
                "https://cf-hls-media.sndcdn.com/media/playlist.m3u8",
                Vec::new(),
                None,
            ),
            TrackMetadata::new(
                "Flickermood",
                "Forss",
                "https://soundcloud.com/forss/flickermood",
                None,
                None,
            ),
        );

        assert_eq!(soundcloud_track_id(&request), Some(293));
    }

    #[test]
    fn only_falls_back_to_html_for_parse_or_not_found_failures() {
        let parse_error = ResolveError::Parse("bad payload".to_owned());
        assert_eq!(
            classify_probe_error(&parse_error),
            SoundCloudProbeAction::FallbackToHtml
        );

        assert_eq!(
            classify_probe_status(Some(StatusCode::NOT_FOUND)),
            SoundCloudProbeAction::FallbackToHtml
        );
        assert_eq!(
            classify_probe_status(Some(StatusCode::TOO_MANY_REQUESTS)),
            SoundCloudProbeAction::Fail
        );
    }
}
