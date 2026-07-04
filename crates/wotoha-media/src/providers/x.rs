use std::{
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use regex::Regex;
use reqwest::{Client, Url, header::HeaderMap};
use serde_json::{Value, json};
use tokio::sync::{Mutex, RwLock};
use wotoha_core::{PreparedSource, TrackMetadata, TrackRequest};

use crate::{ResolveError, provider::MediaProvider};

const X_GUEST_ACTIVATE_URL: &str = "https://api.twitter.com/1.1/guest/activate.json";
const X_WEB_CONTEXT_TTL: Duration = Duration::from_secs(6 * 60 * 60);
const X_GUEST_TOKEN_TTL: Duration = Duration::from_secs(20 * 60);

#[derive(Clone, Debug)]
pub struct XProvider {
    web_context: Arc<RwLock<Option<CachedWebContext>>>,
    guest_token: Arc<RwLock<Option<CachedGuestToken>>>,
    web_context_refresh: Arc<Mutex<()>>,
    guest_token_refresh: Arc<Mutex<()>>,
}

#[derive(Clone, Debug)]
struct XWebContext {
    bearer_token: String,
    query_id: String,
}

#[derive(Clone, Debug)]
struct CachedWebContext {
    context: XWebContext,
    cached_at: Instant,
}

#[derive(Clone, Debug)]
struct CachedGuestToken {
    bearer_token: String,
    guest_token: String,
    cached_at: Instant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum XPayloadRetryAction {
    RefreshGuestToken,
    RefreshWebContext,
    Fail,
}

impl Default for XProvider {
    fn default() -> Self {
        Self {
            web_context: Arc::new(RwLock::new(None)),
            guest_token: Arc::new(RwLock::new(None)),
            web_context_refresh: Arc::new(Mutex::new(())),
            guest_token_refresh: Arc::new(Mutex::new(())),
        }
    }
}

#[async_trait]
impl MediaProvider for XProvider {
    fn id(&self) -> &'static str {
        "x"
    }

    fn supports(&self, raw_url: &str) -> bool {
        let Ok(url) = Url::parse(raw_url) else {
            return false;
        };

        let host = match url.host_str() {
            Some(host) => host.to_ascii_lowercase(),
            None => return false,
        };
        if !matches!(
            host.as_str(),
            "twitter.com"
                | "www.twitter.com"
                | "mobile.twitter.com"
                | "x.com"
                | "www.x.com"
                | "mobile.x.com"
        ) {
            return false;
        }

        url.path_segments()
            .map(|segments| segments.collect::<Vec<_>>())
            .map(|segments| segments.len() >= 3 && segments[1] == "status")
            .unwrap_or(false)
    }

    async fn warmup(&self, probe_client: &Client) -> Result<(), ResolveError> {
        let context = self.web_context(probe_client, "https://x.com/").await?;
        let _ = self
            .guest_token(probe_client, &context.bearer_token)
            .await?;
        Ok(())
    }

    async fn probe(
        &self,
        raw_url: &str,
        probe_client: &Client,
    ) -> Result<TrackRequest, ResolveError> {
        let tweet_id = extract_tweet_id(raw_url)
            .ok_or_else(|| ResolveError::Parse("missing X tweet id".to_owned()))?;
        let payload = self
            .fetch_tweet_payload_with_retry(probe_client, raw_url, &tweet_id)
            .await?;
        let result = payload
            .get("data")
            .and_then(|value| value.get("tweetResult"))
            .and_then(|value| value.get("result"))
            .ok_or_else(|| ResolveError::Parse("missing X tweet result".to_owned()))?;

        let media = find_first_media(result).ok_or_else(|| {
            ResolveError::Parse("X tweet did not expose playable media".to_owned())
        })?;
        let variant = pick_best_variant(media).ok_or_else(|| {
            ResolveError::Parse("X media did not expose a playable variant".to_owned())
        })?;

        let screen_name = result
            .get("core")
            .and_then(|value| value.get("user_results"))
            .and_then(|value| value.get("result"))
            .and_then(|value| value.get("legacy"))
            .and_then(|value| value.get("screen_name"))
            .and_then(Value::as_str)
            .unwrap_or("i");
        let canonical_url = format!("https://x.com/{screen_name}/status/{tweet_id}");
        Ok(track_request_from_result(
            raw_url,
            &tweet_id,
            &canonical_url,
            result,
            media,
            &variant,
        ))
    }

    async fn refresh_playback(
        &self,
        request: &TrackRequest,
        probe_client: &Client,
    ) -> Result<Option<TrackRequest>, ResolveError> {
        let tweet_id = extract_tweet_id(request.canonical_url.as_ref())
            .or_else(|| extract_tweet_id(request.requested_url.as_ref()))
            .ok_or_else(|| ResolveError::Parse("missing X tweet id".to_owned()))?;
        let payload = self
            .fetch_tweet_payload_with_retry(probe_client, request.requested_url.as_ref(), &tweet_id)
            .await?;
        let result = payload
            .get("data")
            .and_then(|value| value.get("tweetResult"))
            .and_then(|value| value.get("result"))
            .ok_or_else(|| ResolveError::Parse("missing X tweet result".to_owned()))?;
        let media = find_first_media(result).ok_or_else(|| {
            ResolveError::Parse("X tweet did not expose playable media".to_owned())
        })?;
        let variant = pick_best_variant(media).ok_or_else(|| {
            ResolveError::Parse("X media did not expose a playable variant".to_owned())
        })?;

        Ok(Some(track_request_from_existing_request(
            request, &tweet_id, result, media, &variant,
        )))
    }
}

#[derive(Clone, Debug)]
struct VideoVariant {
    content_type: String,
    url: String,
    bitrate: Option<u64>,
}

impl XProvider {
    async fn fetch_tweet_payload_with_retry(
        &self,
        probe_client: &Client,
        raw_url: &str,
        tweet_id: &str,
    ) -> Result<Value, ResolveError> {
        let mut force_context_refresh = false;
        let mut force_guest_refresh = false;
        let mut last_error = None;

        for _ in 0..3 {
            let context = if force_context_refresh {
                self.refresh_web_context(probe_client, raw_url).await?
            } else {
                self.web_context(probe_client, raw_url).await?
            };
            let guest_token = if force_guest_refresh {
                self.refresh_guest_token(probe_client, &context.bearer_token)
                    .await?
            } else {
                self.guest_token(probe_client, &context.bearer_token)
                    .await?
            };

            match fetch_tweet_payload(
                probe_client,
                &context.query_id,
                &context.bearer_token,
                &guest_token,
                tweet_id,
            )
            .await
            {
                Ok(payload) => return Ok(payload),
                Err(error) => {
                    let action =
                        classify_payload_retry(&error, force_guest_refresh, force_context_refresh);
                    match action {
                        XPayloadRetryAction::RefreshGuestToken => {
                            last_error = Some(error);
                            force_guest_refresh = true;
                            continue;
                        }
                        XPayloadRetryAction::RefreshWebContext => {
                            last_error = Some(error);
                            force_context_refresh = true;
                            force_guest_refresh = true;
                            continue;
                        }
                        XPayloadRetryAction::Fail => {
                            return Err(error);
                        }
                    }
                }
            }
        }

        Err(last_error
            .unwrap_or_else(|| ResolveError::Parse("failed to resolve X tweet payload".to_owned())))
    }

    async fn web_context(
        &self,
        probe_client: &Client,
        raw_url: &str,
    ) -> Result<XWebContext, ResolveError> {
        if let Some(cached) = self.web_context.read().await.clone()
            && cached.cached_at.elapsed() < X_WEB_CONTEXT_TTL
        {
            return Ok(cached.context);
        }

        let _refresh = self.web_context_refresh.lock().await;
        if let Some(cached) = self.web_context.read().await.clone()
            && cached.cached_at.elapsed() < X_WEB_CONTEXT_TTL
        {
            return Ok(cached.context);
        }

        self.fetch_web_context(probe_client, raw_url).await
    }

    async fn refresh_web_context(
        &self,
        probe_client: &Client,
        raw_url: &str,
    ) -> Result<XWebContext, ResolveError> {
        let _refresh = self.web_context_refresh.lock().await;
        self.fetch_web_context(probe_client, raw_url).await
    }

    async fn fetch_web_context(
        &self,
        probe_client: &Client,
        raw_url: &str,
    ) -> Result<XWebContext, ResolveError> {
        let page = probe_client
            .get(raw_url)
            .send()
            .await
            .map_err(ResolveError::Request)?
            .error_for_status()
            .map_err(ResolveError::Request)?
            .text()
            .await
            .map_err(ResolveError::Request)?;
        let main_js_url = extract_main_js_url(&page)
            .ok_or_else(|| ResolveError::Parse("missing X main.js URL".to_owned()))?;
        let main_js = probe_client
            .get(&main_js_url)
            .send()
            .await
            .map_err(ResolveError::Request)?
            .error_for_status()
            .map_err(ResolveError::Request)?
            .text()
            .await
            .map_err(ResolveError::Request)?;

        let context = XWebContext {
            bearer_token: extract_bearer_token(&main_js)
                .ok_or_else(|| ResolveError::Parse("missing X bearer token".to_owned()))?,
            query_id: extract_query_id(&main_js).ok_or_else(|| {
                ResolveError::Parse("missing X TweetResultByRestId query id".to_owned())
            })?,
        };
        *self.web_context.write().await = Some(CachedWebContext {
            context: context.clone(),
            cached_at: Instant::now(),
        });
        Ok(context)
    }

    async fn guest_token(
        &self,
        probe_client: &Client,
        bearer_token: &str,
    ) -> Result<String, ResolveError> {
        if let Some(cached) = self.guest_token.read().await.clone()
            && cached.bearer_token == bearer_token
            && cached.cached_at.elapsed() < X_GUEST_TOKEN_TTL
        {
            return Ok(cached.guest_token);
        }

        let _refresh = self.guest_token_refresh.lock().await;
        if let Some(cached) = self.guest_token.read().await.clone()
            && cached.bearer_token == bearer_token
            && cached.cached_at.elapsed() < X_GUEST_TOKEN_TTL
        {
            return Ok(cached.guest_token);
        }

        self.fetch_guest_token(probe_client, bearer_token).await
    }

    async fn refresh_guest_token(
        &self,
        probe_client: &Client,
        bearer_token: &str,
    ) -> Result<String, ResolveError> {
        let _refresh = self.guest_token_refresh.lock().await;
        self.fetch_guest_token(probe_client, bearer_token).await
    }

    async fn fetch_guest_token(
        &self,
        probe_client: &Client,
        bearer_token: &str,
    ) -> Result<String, ResolveError> {
        let guest_token = activate_guest_token(probe_client, bearer_token).await?;
        *self.guest_token.write().await = Some(CachedGuestToken {
            bearer_token: bearer_token.to_owned(),
            guest_token: guest_token.clone(),
            cached_at: Instant::now(),
        });
        Ok(guest_token)
    }
}

fn track_request_from_result(
    raw_url: &str,
    tweet_id: &str,
    canonical_url: &str,
    result: &Value,
    media: &serde_json::Map<String, Value>,
    variant: &VideoVariant,
) -> TrackRequest {
    let title = extract_title(result, canonical_url);
    let author = result
        .get("core")
        .and_then(|value| value.get("user_results"))
        .and_then(|value| value.get("result"))
        .and_then(|value| value.get("core"))
        .and_then(|value| value.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("X");
    let thumbnail = media
        .get("media_url_https")
        .and_then(Value::as_str)
        .map(Arc::<str>::from);
    let duration = extract_duration(media);
    let prepared =
        if variant.content_type == "application/x-mpegURL" || variant.url.contains(".m3u8") {
            PreparedSource::hls(variant.url.clone(), Vec::new(), None)
        } else {
            PreparedSource::http(variant.url.clone(), Vec::new(), None, None)
        };

    TrackRequest::new(
        "x",
        format!("x:tweet:{tweet_id}"),
        raw_url.to_owned(),
        canonical_url.to_owned(),
        canonical_url.to_owned(),
        prepared,
        TrackMetadata::new(
            title,
            author.to_owned(),
            canonical_url.to_owned(),
            thumbnail,
            duration,
        ),
    )
}

fn track_request_from_existing_request(
    request: &TrackRequest,
    tweet_id: &str,
    result: &Value,
    media: &serde_json::Map<String, Value>,
    variant: &VideoVariant,
) -> TrackRequest {
    let canonical_url = request.canonical_url.as_ref();
    let title = extract_title(result, canonical_url);
    let thumbnail = media
        .get("media_url_https")
        .and_then(Value::as_str)
        .map(Arc::<str>::from)
        .or_else(|| request.metadata.thumbnail_url.clone());
    let duration = extract_duration(media).or(request.metadata.duration);
    let prepared =
        if variant.content_type == "application/x-mpegURL" || variant.url.contains(".m3u8") {
            PreparedSource::hls(variant.url.clone(), Vec::new(), None)
        } else {
            PreparedSource::http(variant.url.clone(), Vec::new(), None, None)
        };

    TrackRequest::new(
        "x",
        format!("x:tweet:{tweet_id}"),
        request.requested_url.clone(),
        request.canonical_url.clone(),
        request.canonical_url.clone(),
        prepared,
        TrackMetadata::new(
            title,
            request.metadata.author.clone(),
            request.metadata.uri.clone(),
            thumbnail,
            duration,
        ),
    )
}

async fn activate_guest_token(
    probe_client: &Client,
    bearer_token: &str,
) -> Result<String, ResolveError> {
    let payload: Value = probe_client
        .post(X_GUEST_ACTIVATE_URL)
        .headers(token_headers(bearer_token, None)?)
        .send()
        .await
        .map_err(ResolveError::Request)?
        .error_for_status()
        .map_err(ResolveError::Request)?
        .json()
        .await
        .map_err(ResolveError::Request)?;

    payload
        .get("guest_token")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| ResolveError::Parse("missing X guest token".to_owned()))
}

async fn fetch_tweet_payload(
    probe_client: &Client,
    query_id: &str,
    bearer_token: &str,
    guest_token: &str,
    tweet_id: &str,
) -> Result<Value, ResolveError> {
    let variables = json!({
        "tweetId": tweet_id,
        "withCommunity": true,
        "includePromotedContent": true,
        "withVoice": true,
    });
    let features = json!({
        "responsive_web_graphql_exclude_directive_enabled": true,
        "verified_phone_label_enabled": false,
        "responsive_web_graphql_timeline_navigation_enabled": true,
        "responsive_web_graphql_skip_user_profile_image_extensions_enabled": false,
        "tweetypie_unmention_optimization_enabled": true,
        "responsive_web_edit_tweet_api_enabled": true,
        "graphql_is_translatable_rweb_tweet_is_translatable_enabled": true,
        "view_counts_everywhere_api_enabled": true,
        "longform_notetweets_consumption_enabled": true,
        "responsive_web_twitter_article_tweet_consumption_enabled": true,
        "tweet_awards_web_tipping_enabled": false,
        "creator_subscriptions_quote_tweet_preview_enabled": false,
        "freedom_of_speech_not_reach_fetch_enabled": true,
        "standardized_nudges_misinfo": true,
        "tweet_with_visibility_results_prefer_gql_limited_actions_policy_enabled": true,
        "longform_notetweets_richtext_consumption_enabled": true,
        "longform_notetweets_inline_media_enabled": true,
        "responsive_web_media_download_video_enabled": false,
        "responsive_web_enhance_cards_enabled": false,
    });

    let url = format!(
        "https://x.com/i/api/graphql/{query_id}/TweetResultByRestId?variables={}&features={}",
        urlencoding(&variables.to_string()),
        urlencoding(&features.to_string()),
    );

    probe_client
        .get(url)
        .headers(token_headers(bearer_token, Some(guest_token))?)
        .send()
        .await
        .map_err(ResolveError::Request)?
        .error_for_status()
        .map_err(ResolveError::Request)?
        .json()
        .await
        .map_err(ResolveError::Request)
}

fn token_headers(bearer_token: &str, guest_token: Option<&str>) -> Result<HeaderMap, ResolveError> {
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        format!("Bearer {bearer_token}")
            .parse()
            .map_err(|_| ResolveError::InvalidHeaderValue("invalid X bearer token".to_owned()))?,
    );
    headers.insert(
        "accept",
        "*/*"
            .parse()
            .expect("accept should be a valid header value"),
    );
    headers.insert(
        "accept-language",
        "en-US,en;q=0.9"
            .parse()
            .expect("accept-language should be a valid header value"),
    );
    headers.insert(
        "user-agent",
        "Mozilla/5.0"
            .parse()
            .expect("user-agent should be a valid header value"),
    );
    headers.insert(
        "x-twitter-active-user",
        "yes"
            .parse()
            .expect("x-twitter-active-user should be a valid header value"),
    );
    headers.insert(
        "x-twitter-client-language",
        "en".parse()
            .expect("x-twitter-client-language should be a valid header value"),
    );

    if let Some(guest_token) = guest_token {
        headers.insert(
            "x-guest-token",
            guest_token.parse().map_err(|_| {
                ResolveError::InvalidHeaderValue("invalid X guest token".to_owned())
            })?,
        );
    }

    Ok(headers)
}

fn find_first_media(value: &Value) -> Option<&serde_json::Map<String, Value>> {
    find_primary_media(value).or_else(|| find_any_media(value))
}

fn find_primary_media(value: &Value) -> Option<&serde_json::Map<String, Value>> {
    media_from_path(value, &["legacy", "extended_entities", "media"])
        .or_else(|| media_from_path(value, &["legacy", "entities", "media"]))
}

fn media_from_path<'a>(
    value: &'a Value,
    path: &[&str],
) -> Option<&'a serde_json::Map<String, Value>> {
    let mut current = value;
    for segment in path {
        current = current.get(*segment)?;
    }

    current.as_array()?.iter().find_map(media_object)
}

fn media_object(value: &Value) -> Option<&serde_json::Map<String, Value>> {
    let map = value.as_object()?;
    map.get("video_info")
        .and_then(|entry| entry.get("variants"))
        .and_then(Value::as_array)?;
    Some(map)
}

fn find_any_media(value: &Value) -> Option<&serde_json::Map<String, Value>> {
    match value {
        Value::Object(map) => {
            if media_object(value).is_some() {
                return Some(map);
            }

            map.values().find_map(find_any_media)
        }
        Value::Array(items) => items.iter().find_map(find_any_media),
        _ => None,
    }
}

fn pick_best_variant(media: &serde_json::Map<String, Value>) -> Option<VideoVariant> {
    let variants = media
        .get("video_info")
        .and_then(|value| value.get("variants"))
        .and_then(Value::as_array)?;

    let mut parsed: Vec<VideoVariant> = variants
        .iter()
        .filter_map(|variant| {
            Some(VideoVariant {
                content_type: variant.get("content_type")?.as_str()?.to_owned(),
                url: variant.get("url")?.as_str()?.to_owned(),
                bitrate: variant.get("bitrate").and_then(Value::as_u64),
            })
        })
        .collect();

    parsed.sort_by(|left, right| {
        right
            .bitrate
            .unwrap_or_default()
            .cmp(&left.bitrate.unwrap_or_default())
    });

    parsed
        .iter()
        .find(|variant| variant.content_type == "application/x-mpegURL")
        .cloned()
        .or_else(|| {
            parsed
                .iter()
                .find(|variant| variant.content_type == "video/mp4")
                .cloned()
        })
        .or_else(|| parsed.into_iter().next())
}

fn extract_title(result: &Value, canonical_url: &str) -> String {
    let raw = result
        .get("legacy")
        .and_then(|value| value.get("full_text"))
        .and_then(Value::as_str)
        .unwrap_or(canonical_url);
    sanitize_title(raw).unwrap_or_else(|| canonical_url.to_owned())
}

fn sanitize_title(raw: &str) -> Option<String> {
    static URL_RE: OnceLock<Regex> = OnceLock::new();
    let without_urls = URL_RE
        .get_or_init(|| Regex::new(r"https?://\S+").expect("X URL regex should compile"))
        .replace_all(raw, "");
    let title = without_urls
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    (!title.is_empty()).then_some(title)
}

fn extract_duration(media: &serde_json::Map<String, Value>) -> Option<Duration> {
    media
        .get("video_info")
        .and_then(|value| value.get("duration_millis"))
        .and_then(Value::as_u64)
        .map(Duration::from_millis)
}

fn extract_main_js_url(page: &str) -> Option<String> {
    static MAIN_JS_RE: OnceLock<Regex> = OnceLock::new();
    MAIN_JS_RE
        .get_or_init(|| {
            Regex::new(
                r#"https://abs\.twimg\.com/responsive-web/client-web(?:-legacy)?/main\.[^.]+\.js"#,
            )
            .expect("X main.js regex should compile")
        })
        .find(page)
        .map(|match_| match_.as_str().to_owned())
}

fn extract_bearer_token(main_js: &str) -> Option<String> {
    static BEARER_RE: OnceLock<Regex> = OnceLock::new();
    BEARER_RE
        .get_or_init(|| {
            Regex::new(r#"AAAAAAAAA[^"']+"#).expect("X bearer token regex should compile")
        })
        .find(main_js)
        .map(|match_| match_.as_str().to_owned())
}

fn extract_query_id(main_js: &str) -> Option<String> {
    static QUERY_RE: OnceLock<Regex> = OnceLock::new();
    QUERY_RE
        .get_or_init(|| {
            Regex::new(r#"queryId:"([A-Za-z0-9_-]{20,})",operationName:"TweetResultByRestId""#)
                .expect("X query id regex should compile")
        })
        .captures(main_js)
        .and_then(|captures| captures.get(1))
        .map(|match_| match_.as_str().to_owned())
}

fn extract_tweet_id(raw_url: &str) -> Option<String> {
    let url = Url::parse(raw_url).ok()?;
    let segments: Vec<_> = url.path_segments()?.collect();
    if segments.len() < 3 || segments[1] != "status" {
        return None;
    }

    Some(segments[2].to_owned())
}

fn urlencoding(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

fn classify_payload_retry(
    error: &ResolveError,
    guest_refreshed: bool,
    context_refreshed: bool,
) -> XPayloadRetryAction {
    match error {
        ResolveError::Request(request_error) => {
            classify_payload_status(request_error.status(), guest_refreshed, context_refreshed)
        }
        _ => XPayloadRetryAction::Fail,
    }
}

fn classify_payload_status(
    status: Option<reqwest::StatusCode>,
    guest_refreshed: bool,
    context_refreshed: bool,
) -> XPayloadRetryAction {
    match status {
        Some(reqwest::StatusCode::UNAUTHORIZED) | Some(reqwest::StatusCode::FORBIDDEN) => {
            if !guest_refreshed {
                XPayloadRetryAction::RefreshGuestToken
            } else if !context_refreshed {
                XPayloadRetryAction::RefreshWebContext
            } else {
                XPayloadRetryAction::Fail
            }
        }
        _ => XPayloadRetryAction::Fail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_tweet_id_from_status_url() {
        assert_eq!(
            extract_tweet_id("https://x.com/example/status/1234567890/video/1"),
            Some("1234567890".to_owned())
        );
    }

    #[test]
    fn prefers_hls_variant_for_audio_playback() {
        let media = serde_json::from_str::<Value>(
            r#"{
                "video_info": {
                    "variants": [
                        { "content_type": "application/x-mpegURL", "url": "https://example.com/master.m3u8" },
                        { "content_type": "video/mp4", "url": "https://example.com/low.mp4", "bitrate": 256000 },
                        { "content_type": "video/mp4", "url": "https://example.com/high.mp4", "bitrate": 832000 }
                    ]
                }
            }"#,
        )
        .unwrap();

        let variant = pick_best_variant(media.as_object().unwrap()).unwrap();
        assert_eq!(variant.url, "https://example.com/master.m3u8");
    }

    #[test]
    fn falls_back_to_highest_bitrate_mp4_without_hls() {
        let media = serde_json::from_str::<Value>(
            r#"{
                "video_info": {
                    "variants": [
                        { "content_type": "video/mp4", "url": "https://example.com/low.mp4", "bitrate": 256000 },
                        { "content_type": "video/mp4", "url": "https://example.com/high.mp4", "bitrate": 832000 }
                    ]
                }
            }"#,
        )
        .unwrap();

        let variant = pick_best_variant(media.as_object().unwrap()).unwrap();
        assert_eq!(variant.url, "https://example.com/high.mp4");
    }

    #[test]
    fn sanitizes_tweet_title_urls_and_line_breaks() {
        let title = sanitize_title(
            "やっぱりホンモノは一味違うな…\n見れば見るほどクセになる人柄すぎる https://t.co/qT0U2kyGK8",
        )
        .unwrap();
        assert_eq!(
            title,
            "やっぱりホンモノは一味違うな… 見れば見るほどクセになる人柄すぎる"
        );
    }

    #[test]
    fn extracts_duration_from_video_info() {
        let media = serde_json::from_str::<Value>(
            r#"{
                "video_info": {
                    "duration_millis": 12345,
                    "variants": [
                        { "content_type": "video/mp4", "url": "https://example.com/video.mp4", "bitrate": 832000 }
                    ]
                }
            }"#,
        )
        .unwrap();

        assert_eq!(
            extract_duration(media.as_object().unwrap()),
            Some(Duration::from_millis(12345))
        );
    }

    #[test]
    fn prefers_primary_tweet_media_before_nested_media() {
        let payload = serde_json::from_str::<Value>(
            r#"{
                "card": {
                    "binding_values": {
                        "player": {
                            "video_info": {
                                "variants": [
                                    { "content_type": "video/mp4", "url": "https://example.com/nested.mp4", "bitrate": 832000 }
                                ]
                            }
                        }
                    }
                },
                "legacy": {
                    "extended_entities": {
                        "media": [
                            {
                                "video_info": {
                                    "variants": [
                                        { "content_type": "video/mp4", "url": "https://example.com/primary.mp4", "bitrate": 256000 }
                                    ]
                                }
                            }
                        ]
                    }
                }
            }"#,
        )
        .unwrap();

        let media = find_first_media(&payload).unwrap();
        let variant = pick_best_variant(media).unwrap();

        assert_eq!(variant.url, "https://example.com/primary.mp4");
    }

    #[test]
    fn only_retries_x_payload_on_auth_failures() {
        let parse_error = ResolveError::Parse("bad payload".to_owned());
        assert_eq!(
            classify_payload_retry(&parse_error, false, false),
            XPayloadRetryAction::Fail
        );
        assert_eq!(
            classify_payload_status(Some(reqwest::StatusCode::UNAUTHORIZED), false, false,),
            XPayloadRetryAction::RefreshGuestToken
        );
        assert_eq!(
            classify_payload_status(Some(reqwest::StatusCode::FORBIDDEN), true, false,),
            XPayloadRetryAction::RefreshWebContext
        );
        assert_eq!(
            classify_payload_status(Some(reqwest::StatusCode::TOO_MANY_REQUESTS), false, false,),
            XPayloadRetryAction::Fail
        );
    }
}
