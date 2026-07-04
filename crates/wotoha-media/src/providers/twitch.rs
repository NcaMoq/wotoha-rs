use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use reqwest::{Client, Url};
use serde_json::{Value, json};
use wotoha_core::{PreparedSource, TrackMetadata, TrackRequest};

use crate::{ResolveError, provider::MediaProvider};

const TWITCH_GQL_URL: &str = "https://gql.twitch.tv/gql";
const TWITCH_WEB_CLIENT_ID: &str = "kimne78kx3ncx6brgo4mv6wki5h1ko";
const TWITCH_PLAYER_TYPE: &str = "site";
const TWITCH_PLATFORM: &str = "web";
const TWITCH_CHANNEL_QUERY: &str = r#"query ChannelMetadata($login: String!) { user(login: $login) { login displayName profileImageURL(width: 300) stream { id title previewImageURL(width: 640, height: 360) type createdAt } } }"#;
const TWITCH_VIDEO_QUERY: &str = r#"query VideoMetadata($id: ID!) { video(id: $id) { id title previewThumbnailURL owner { displayName login profileImageURL(width: 300) } lengthSeconds } }"#;
const TWITCH_PLAYBACK_QUERY: &str = r#"query PlaybackAccessToken_Template($login: String!, $isLive: Boolean!, $vodID: ID!, $isVod: Boolean!, $playerType: String!, $platform: String!) { streamPlaybackAccessToken(channelName: $login, params: { platform: $platform, playerBackend: "mediaplayer", playerType: $playerType }) @include(if: $isLive) { value signature authorization { isForbidden forbiddenReasonCode } } videoPlaybackAccessToken(id: $vodID, params: { platform: $platform, playerBackend: "mediaplayer", playerType: $playerType }) @include(if: $isVod) { value signature } }"#;
const TWITCH_RESERVED_CHANNEL_SEGMENTS: &[&str] = &[
    "activate",
    "bits",
    "bits-checkout",
    "directory",
    "downloads",
    "following",
    "jobs",
    "login",
    "logout",
    "manager",
    "messages",
    "p",
    "payments",
    "popout",
    "prime",
    "search",
    "settings",
    "signup",
    "store",
    "subs",
    "subscriptions",
    "turbo",
    "user",
    "videos",
    "wallet",
];

#[derive(Clone, Debug, Default)]
pub struct TwitchProvider;

#[derive(Clone, Debug, Eq, PartialEq)]
enum TwitchTarget {
    Channel { login: String },
    Video { id: String },
}

#[async_trait]
impl MediaProvider for TwitchProvider {
    fn id(&self) -> &'static str {
        "twitch"
    }

    fn supports(&self, raw_url: &str) -> bool {
        parse_twitch_target(raw_url).is_some()
    }

    async fn probe(
        &self,
        raw_url: &str,
        probe_client: &Client,
    ) -> Result<TrackRequest, ResolveError> {
        let target = parse_twitch_target(raw_url)
            .ok_or_else(|| ResolveError::UnsupportedSource(raw_url.to_owned()))?;

        match target {
            TwitchTarget::Channel { login } => probe_channel(raw_url, &login, probe_client).await,
            TwitchTarget::Video { id } => probe_video(raw_url, &id, probe_client).await,
        }
    }

    async fn refresh_playback(
        &self,
        request: &TrackRequest,
        probe_client: &Client,
    ) -> Result<Option<TrackRequest>, ResolveError> {
        let Some(target) = twitch_target_from_request(request) else {
            return Ok(None);
        };

        let refreshed = match target {
            TwitchTarget::Channel { login } => {
                refresh_channel_playback(request, &login, probe_client).await?
            }
            TwitchTarget::Video { id } => {
                refresh_video_playback(request, &id, probe_client).await?
            }
        };

        Ok(Some(refreshed))
    }
}

async fn probe_channel(
    raw_url: &str,
    login: &str,
    probe_client: &Client,
) -> Result<TrackRequest, ResolveError> {
    let (metadata_payload, token_payload) = tokio::try_join!(
        twitch_graphql(
            probe_client,
            json!({
                "operationName": "ChannelMetadata",
                "query": TWITCH_CHANNEL_QUERY,
                "variables": { "login": login },
            }),
        ),
        twitch_graphql(
            probe_client,
            json!({
                "operationName": "PlaybackAccessToken_Template",
                "query": TWITCH_PLAYBACK_QUERY,
                "variables": {
                    "login": login,
                    "isLive": true,
                    "vodID": "",
                    "isVod": false,
                    "playerType": TWITCH_PLAYER_TYPE,
                    "platform": TWITCH_PLATFORM,
                },
            }),
        ),
    )?;

    let user = metadata_payload
        .get("data")
        .and_then(|value| value.get("user"))
        .ok_or_else(|| ResolveError::Parse("missing Twitch user payload".to_owned()))?;
    let stream = user
        .get("stream")
        .ok_or_else(|| ResolveError::Parse("Twitch channel is not currently live".to_owned()))?;
    if stream.is_null() {
        return Err(ResolveError::Parse(
            "Twitch channel is not currently live".to_owned(),
        ));
    }

    let token = token_payload
        .get("data")
        .and_then(|value| value.get("streamPlaybackAccessToken"))
        .ok_or_else(|| ResolveError::Parse("missing Twitch stream access token".to_owned()))?;
    ensure_not_forbidden(token)?;
    let signature = token
        .get("signature")
        .and_then(Value::as_str)
        .ok_or_else(|| ResolveError::Parse("missing Twitch stream signature".to_owned()))?;
    let token_value = token
        .get("value")
        .and_then(Value::as_str)
        .ok_or_else(|| ResolveError::Parse("missing Twitch stream token".to_owned()))?;

    let login = user
        .get("login")
        .and_then(Value::as_str)
        .unwrap_or(login)
        .to_owned();
    let canonical_url = format!("https://www.twitch.tv/{login}");
    let master_url = live_playlist_url(&login, signature, token_value);
    let audio_only_url =
        audio_only_playlist_or_master(probe_client, &master_url, &canonical_url).await;
    let expires_at_unix = token_expiry(token_value);

    Ok(TrackRequest::new(
        "twitch",
        format!("twitch:channel:{login}"),
        raw_url.to_owned(),
        canonical_url.clone(),
        audio_only_url.clone(),
        PreparedSource::hls(audio_only_url, Vec::new(), expires_at_unix),
        TrackMetadata::new(
            stream
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or(&canonical_url)
                .to_owned(),
            user.get("displayName")
                .and_then(Value::as_str)
                .unwrap_or("Twitch")
                .to_owned(),
            canonical_url,
            stream
                .get("previewImageURL")
                .and_then(Value::as_str)
                .or_else(|| user.get("profileImageURL").and_then(Value::as_str))
                .map(normalize_twitch_thumbnail)
                .map(Arc::<str>::from),
            None,
        ),
    ))
}

async fn probe_video(
    raw_url: &str,
    video_id: &str,
    probe_client: &Client,
) -> Result<TrackRequest, ResolveError> {
    let (metadata_payload, token_payload) = tokio::try_join!(
        twitch_graphql(
            probe_client,
            json!({
                "operationName": "VideoMetadata",
                "query": TWITCH_VIDEO_QUERY,
                "variables": { "id": video_id },
            }),
        ),
        twitch_graphql(
            probe_client,
            json!({
                "operationName": "PlaybackAccessToken_Template",
                "query": TWITCH_PLAYBACK_QUERY,
                "variables": {
                    "login": "",
                    "isLive": false,
                    "vodID": video_id,
                    "isVod": true,
                    "playerType": TWITCH_PLAYER_TYPE,
                    "platform": TWITCH_PLATFORM,
                },
            }),
        ),
    )?;

    let video = metadata_payload
        .get("data")
        .and_then(|value| value.get("video"))
        .ok_or_else(|| ResolveError::Parse("missing Twitch video payload".to_owned()))?;
    let owner = video
        .get("owner")
        .ok_or_else(|| ResolveError::Parse("missing Twitch video owner".to_owned()))?;

    let token = token_payload
        .get("data")
        .and_then(|value| value.get("videoPlaybackAccessToken"))
        .ok_or_else(|| ResolveError::Parse("missing Twitch video access token".to_owned()))?;
    let signature = token
        .get("signature")
        .and_then(Value::as_str)
        .ok_or_else(|| ResolveError::Parse("missing Twitch video signature".to_owned()))?;
    let token_value = token
        .get("value")
        .and_then(Value::as_str)
        .ok_or_else(|| ResolveError::Parse("missing Twitch video token".to_owned()))?;

    let canonical_url = format!("https://www.twitch.tv/videos/{video_id}");
    let master_url = vod_playlist_url(video_id, signature, token_value);
    let audio_only_url =
        audio_only_playlist_or_master(probe_client, &master_url, &canonical_url).await;
    let expires_at_unix = token_expiry(token_value);

    Ok(TrackRequest::new(
        "twitch",
        format!("twitch:video:{video_id}"),
        raw_url.to_owned(),
        canonical_url.clone(),
        audio_only_url.clone(),
        PreparedSource::hls(audio_only_url, Vec::new(), expires_at_unix),
        TrackMetadata::new(
            video
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or(&canonical_url)
                .to_owned(),
            owner
                .get("displayName")
                .and_then(Value::as_str)
                .unwrap_or("Twitch")
                .to_owned(),
            canonical_url,
            video
                .get("previewThumbnailURL")
                .and_then(Value::as_str)
                .map(normalize_twitch_thumbnail)
                .or_else(|| {
                    owner
                        .get("profileImageURL")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .map(Arc::<str>::from),
            video
                .get("lengthSeconds")
                .and_then(Value::as_u64)
                .map(Duration::from_secs),
        ),
    ))
}

async fn refresh_channel_playback(
    request: &TrackRequest,
    login: &str,
    probe_client: &Client,
) -> Result<TrackRequest, ResolveError> {
    let token_payload = twitch_graphql(
        probe_client,
        json!({
            "operationName": "PlaybackAccessToken_Template",
            "query": TWITCH_PLAYBACK_QUERY,
            "variables": {
                "login": login,
                "isLive": true,
                "vodID": "",
                "isVod": false,
                "playerType": TWITCH_PLAYER_TYPE,
                "platform": TWITCH_PLATFORM,
            },
        }),
    )
    .await?;
    refreshed_request_from_token(
        request,
        token_payload,
        TwitchTarget::Channel {
            login: login.to_owned(),
        },
        probe_client,
    )
    .await
}

async fn refresh_video_playback(
    request: &TrackRequest,
    video_id: &str,
    probe_client: &Client,
) -> Result<TrackRequest, ResolveError> {
    let token_payload = twitch_graphql(
        probe_client,
        json!({
            "operationName": "PlaybackAccessToken_Template",
            "query": TWITCH_PLAYBACK_QUERY,
            "variables": {
                "login": "",
                "isLive": false,
                "vodID": video_id,
                "isVod": true,
                "playerType": TWITCH_PLAYER_TYPE,
                "platform": TWITCH_PLATFORM,
            },
        }),
    )
    .await?;
    refreshed_request_from_token(
        request,
        token_payload,
        TwitchTarget::Video {
            id: video_id.to_owned(),
        },
        probe_client,
    )
    .await
}

async fn refreshed_request_from_token(
    request: &TrackRequest,
    token_payload: Value,
    target: TwitchTarget,
    probe_client: &Client,
) -> Result<TrackRequest, ResolveError> {
    let (token, canonical_url, canonical_key, master_url) = match target {
        TwitchTarget::Channel { login } => {
            let token = token_payload
                .get("data")
                .and_then(|value| value.get("streamPlaybackAccessToken"))
                .ok_or_else(|| {
                    ResolveError::Parse("missing Twitch stream access token".to_owned())
                })?;
            ensure_not_forbidden(token)?;
            let signature = token
                .get("signature")
                .and_then(Value::as_str)
                .ok_or_else(|| ResolveError::Parse("missing Twitch stream signature".to_owned()))?;
            let token_value = token
                .get("value")
                .and_then(Value::as_str)
                .ok_or_else(|| ResolveError::Parse("missing Twitch stream token".to_owned()))?;
            (
                token_value.to_owned(),
                format!("https://www.twitch.tv/{login}"),
                format!("twitch:channel:{login}"),
                live_playlist_url(&login, signature, token_value),
            )
        }
        TwitchTarget::Video { id } => {
            let token = token_payload
                .get("data")
                .and_then(|value| value.get("videoPlaybackAccessToken"))
                .ok_or_else(|| {
                    ResolveError::Parse("missing Twitch video access token".to_owned())
                })?;
            let signature = token
                .get("signature")
                .and_then(Value::as_str)
                .ok_or_else(|| ResolveError::Parse("missing Twitch video signature".to_owned()))?;
            let token_value = token
                .get("value")
                .and_then(Value::as_str)
                .ok_or_else(|| ResolveError::Parse("missing Twitch video token".to_owned()))?;
            (
                token_value.to_owned(),
                format!("https://www.twitch.tv/videos/{id}"),
                format!("twitch:video:{id}"),
                vod_playlist_url(&id, signature, token_value),
            )
        }
    };

    let audio_only_url =
        audio_only_playlist_or_master(probe_client, &master_url, &canonical_url).await;
    let expires_at_unix = token_expiry(&token);

    Ok(TrackRequest::new(
        "twitch",
        canonical_key,
        request.requested_url.clone(),
        canonical_url,
        audio_only_url.clone(),
        PreparedSource::hls(audio_only_url, Vec::new(), expires_at_unix),
        request.metadata.clone(),
    ))
}

async fn twitch_graphql(probe_client: &Client, body: Value) -> Result<Value, ResolveError> {
    probe_client
        .post(TWITCH_GQL_URL)
        .header("Client-ID", TWITCH_WEB_CLIENT_ID)
        .header("Accept", "*/*")
        .header("Accept-Language", "en-US")
        .json(&body)
        .send()
        .await
        .map_err(ResolveError::Request)?
        .error_for_status()
        .map_err(ResolveError::Request)?
        .json()
        .await
        .map_err(ResolveError::Request)
}

fn parse_twitch_target(raw_url: &str) -> Option<TwitchTarget> {
    let url = Url::parse(raw_url).ok()?;
    let host = url.host_str()?.to_ascii_lowercase();
    if !matches!(host.as_str(), "twitch.tv" | "www.twitch.tv" | "m.twitch.tv") {
        return None;
    }

    let segments: Vec<_> = url
        .path_segments()?
        .filter(|segment| !segment.is_empty())
        .collect();
    match segments.as_slice() {
        ["videos", id, ..] if id.chars().all(|ch| ch.is_ascii_digit()) => {
            Some(TwitchTarget::Video {
                id: (*id).to_owned(),
            })
        }
        [login] if !TWITCH_RESERVED_CHANNEL_SEGMENTS.contains(login) => {
            Some(TwitchTarget::Channel {
                login: login.to_ascii_lowercase(),
            })
        }
        _ => None,
    }
}

fn twitch_target_from_request(request: &TrackRequest) -> Option<TwitchTarget> {
    if let Some(login) = request.canonical_key.strip_prefix("twitch:channel:") {
        return Some(TwitchTarget::Channel {
            login: login.to_owned(),
        });
    }
    if let Some(id) = request.canonical_key.strip_prefix("twitch:video:") {
        return Some(TwitchTarget::Video { id: id.to_owned() });
    }

    parse_twitch_target(request.requested_url.as_ref())
}

fn live_playlist_url(login: &str, signature: &str, token_value: &str) -> String {
    let mut url = Url::parse(&format!(
        "https://usher.ttvnw.net/api/channel/hls/{login}.m3u8"
    ))
    .expect("Twitch live playlist URL should be valid");
    url.query_pairs_mut()
        .append_pair("allow_source", "true")
        .append_pair("allow_audio_only", "true")
        .append_pair("fast_bread", "true")
        .append_pair("p", &playlist_nonce().to_string())
        .append_pair("player_backend", "mediaplayer")
        .append_pair("playlist_include_framerate", "true")
        .append_pair("reassignments_supported", "true")
        .append_pair("sig", signature)
        .append_pair("supported_codecs", "av1,h265,h264")
        .append_pair("token", token_value)
        .append_pair("player", "twitchweb")
        .append_pair("type", "any");
    url.to_string()
}

fn vod_playlist_url(video_id: &str, signature: &str, token_value: &str) -> String {
    let mut url = Url::parse(&format!("https://usher.ttvnw.net/vod/{video_id}.m3u8"))
        .expect("Twitch VOD playlist URL should be valid");
    url.query_pairs_mut()
        .append_pair("allow_source", "true")
        .append_pair("allow_audio_only", "true")
        .append_pair("nauthsig", signature)
        .append_pair("nauth", token_value)
        .append_pair("player", "twitchweb")
        .append_pair("platform", TWITCH_PLATFORM)
        .append_pair("playlist_include_framerate", "true")
        .append_pair("supported_codecs", "av1,h265,h264");
    url.to_string()
}

fn playlist_nonce() -> u32 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos()
}

fn token_expiry(token_value: &str) -> Option<u64> {
    serde_json::from_str::<Value>(token_value)
        .ok()?
        .get("expires")
        .and_then(Value::as_u64)
}

async fn fetch_audio_only_playlist(
    probe_client: &Client,
    master_url: &str,
    referer: &str,
) -> Result<Option<String>, ResolveError> {
    let manifest = probe_client
        .get(master_url)
        .header("Referer", referer)
        .header("User-Agent", "Mozilla/5.0")
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

async fn audio_only_playlist_or_master(
    probe_client: &Client,
    master_url: &str,
    referer: &str,
) -> String {
    match fetch_audio_only_playlist(probe_client, master_url, referer).await {
        Ok(Some(url)) => url,
        Ok(None) => master_url.to_owned(),
        Err(error) => {
            tracing::warn!(
                error = %error,
                "failed to extract Twitch audio-only playlist; falling back to master playlist"
            );
            master_url.to_owned()
        }
    }
}

fn ensure_not_forbidden(token: &Value) -> Result<(), ResolveError> {
    let Some(authorization) = token.get("authorization") else {
        return Ok(());
    };
    if authorization
        .get("isForbidden")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let reason = authorization
            .get("forbiddenReasonCode")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        return Err(ResolveError::Parse(format!(
            "Twitch playback forbidden: {reason}"
        )));
    }

    Ok(())
}

fn extract_audio_only_playlist(master_url: &str, manifest: &str) -> Option<String> {
    let lines: Vec<_> = manifest
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();

    for (index, line) in lines.iter().enumerate() {
        if !line.starts_with("#EXT-X-STREAM-INF:") {
            continue;
        }
        if !line.contains(r#"VIDEO="audio_only""#) && !looks_like_audio_only_stream(line) {
            continue;
        }

        let uri = lines.get(index + 1)?;
        if uri.starts_with('#') {
            continue;
        }

        return Url::parse(master_url)
            .ok()?
            .join(uri)
            .ok()
            .map(|url| url.to_string());
    }

    None
}

fn looks_like_audio_only_stream(line: &str) -> bool {
    line.contains(r#"CODECS="mp4a."#)
        && !line.contains("avc1")
        && !line.contains("hvc1")
        && !line.contains("hev1")
        && !line.contains("av01")
}

fn normalize_twitch_thumbnail(value: &str) -> String {
    value.replace("{width}", "640").replace("{height}", "360")
}

#[cfg(test)]
mod tests {
    use super::*;
    use wotoha_core::{PreparedSource, TrackMetadata, TrackRequest};

    #[test]
    fn supports_twitch_channels_and_vods_only() {
        let provider = TwitchProvider;

        assert!(provider.supports("https://www.twitch.tv/riotgames"));
        assert!(provider.supports("https://m.twitch.tv/videos/106400740"));
        assert!(!provider.supports("https://www.twitch.tv/directory"));
        assert!(!provider.supports("https://example.com/videos/106400740"));
    }

    #[test]
    fn parses_twitch_targets() {
        assert_eq!(
            parse_twitch_target("https://www.twitch.tv/videos/106400740"),
            Some(TwitchTarget::Video {
                id: "106400740".to_owned(),
            })
        );
        assert_eq!(
            parse_twitch_target("https://www.twitch.tv/RiotGames"),
            Some(TwitchTarget::Channel {
                login: "riotgames".to_owned(),
            })
        );
        assert_eq!(parse_twitch_target("https://www.twitch.tv/directory"), None);
    }

    #[test]
    fn extracts_audio_only_twitch_playlist() {
        let manifest = r#"
        #EXTM3U
        #EXT-X-MEDIA:TYPE=VIDEO,GROUP-ID="audio_only",NAME="Audio Only",AUTOSELECT=NO,DEFAULT=NO
        #EXT-X-STREAM-INF:BANDWIDTH=170704,CODECS="mp4a.40.2",VIDEO="audio_only"
        https://d2nvs31859zcd8.cloudfront.net/twitch/106400740/audio_only/index-dvr.m3u8
        "#;

        let audio_only =
            extract_audio_only_playlist("https://usher.ttvnw.net/vod/106400740.m3u8", manifest)
                .unwrap();

        assert_eq!(
            audio_only,
            "https://d2nvs31859zcd8.cloudfront.net/twitch/106400740/audio_only/index-dvr.m3u8"
        );
    }

    #[test]
    fn parses_twitch_token_expiry() {
        let token = r#"{"expires":1777115651,"vod_id":106400740}"#;
        assert_eq!(token_expiry(token), Some(1777115651));
    }

    #[test]
    fn reports_twitch_forbidden_reason() {
        let token = json!({
            "authorization": {
                "isForbidden": true,
                "forbiddenReasonCode": "SUBSCRIBERS_ONLY"
            }
        });

        let error = ensure_not_forbidden(&token).unwrap_err().to_string();
        assert!(error.contains("SUBSCRIBERS_ONLY"));
    }

    #[test]
    fn derives_twitch_target_from_request_key() {
        let request = TrackRequest::new(
            "twitch",
            "twitch:video:106400740",
            "https://www.twitch.tv/videos/106400740",
            "https://www.twitch.tv/videos/106400740",
            "https://usher.ttvnw.net/vod/106400740.m3u8",
            PreparedSource::hls(
                "https://usher.ttvnw.net/vod/106400740.m3u8",
                Vec::new(),
                None,
            ),
            TrackMetadata::new(
                "vod",
                "owner",
                "https://www.twitch.tv/videos/106400740",
                None,
                None,
            ),
        );

        assert_eq!(
            twitch_target_from_request(&request),
            Some(TwitchTarget::Video {
                id: "106400740".to_owned(),
            })
        );
    }
}
