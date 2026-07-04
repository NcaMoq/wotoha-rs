use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use dashmap::DashMap;
use reqwest::{Client, redirect::Policy};
use thiserror::Error;
use tokio::sync::{Mutex, Semaphore};
use wotoha_contracts::MediaBackend;
use wotoha_core::{
    PreparedSource, TrackRequest,
    url::{
        is_allowed_prepared_url, is_allowed_runtime_redirect_url, is_allowed_track_url,
        summarize_url_for_logs,
    },
};

use crate::{
    provider::MediaProvider,
    providers::{
        BandcampProvider, NiconicoProvider, SoundCloudProvider, TwitchProvider, VimeoProvider,
        XProvider, YouTubeProvider,
    },
};

const PROBE_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const PROBE_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const PROBE_POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(15);
const METADATA_CACHE_TTL: Duration = Duration::from_secs(30 * 60);
const PREPARED_CACHE_TTL: Duration = Duration::from_secs(5 * 60);
const TRANSIENT_PREPARED_CACHE_TTL: Duration = Duration::from_secs(90);
const MAX_CONCURRENT_PROBES: usize = 8;
const PREPARED_REFRESH_SKEW: Duration = Duration::from_secs(60);
const CACHE_MAINTENANCE_INTERVAL: u64 = 128;
const MAX_METADATA_CACHE_ITEMS: usize = 4_096;
const MAX_PREPARED_CACHE_ITEMS: usize = 4_096;
const MAX_URL_ALIAS_ITEMS: usize = 12_288;

#[derive(Clone)]
pub struct MediaResolver {
    inner: Arc<MediaResolverInner>,
}

struct MediaResolverInner {
    providers: Vec<Arc<dyn MediaProvider>>,
    providers_by_id: HashMap<&'static str, Arc<dyn MediaProvider>>,
    probe_client: Client,
    metadata_cache_ttl: Duration,
    metadata_cache: DashMap<String, CachedMetadata>,
    prepared_cache: DashMap<String, CachedPrepared>,
    url_aliases: DashMap<String, CachedAlias>,
    inflight: DashMap<String, Arc<Mutex<()>>>,
    prepare_inflight: DashMap<String, Arc<Mutex<()>>>,
    maintenance_tick: AtomicU64,
    probe_slots: Semaphore,
}

#[derive(Clone)]
struct CachedMetadata {
    request: TrackRequest,
    cached_at: Instant,
}

#[derive(Clone)]
struct CachedPrepared {
    request: TrackRequest,
    cached_at: Instant,
}

#[derive(Clone)]
struct CachedAlias {
    canonical_key: Arc<str>,
    cached_at: Instant,
}

impl MediaResolver {
    pub fn new() -> Result<Self, ResolveError> {
        let x_provider = Arc::new(XProvider::default());
        let providers: Vec<Arc<dyn MediaProvider>> = vec![
            Arc::new(YouTubeProvider),
            Arc::new(SoundCloudProvider::default()),
            Arc::new(BandcampProvider),
            Arc::new(NiconicoProvider),
            Arc::new(VimeoProvider),
            Arc::new(TwitchProvider),
            x_provider,
        ];
        let providers_by_id = providers
            .iter()
            .map(|provider| (provider.id(), provider.clone()))
            .collect();

        let probe_client = Client::builder()
            .user_agent("wotoha-rust/0.1.0")
            .local_address(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
            .connect_timeout(PROBE_CONNECT_TIMEOUT)
            .timeout(PROBE_REQUEST_TIMEOUT)
            .pool_idle_timeout(PROBE_POOL_IDLE_TIMEOUT)
            .pool_max_idle_per_host(8)
            .redirect(Policy::custom(|attempt| {
                if attempt.previous().len() >= 5 {
                    attempt.error("too many redirects")
                } else if wotoha_core::url::is_allowed_track_url(attempt.url().as_str())
                    || is_allowed_runtime_redirect_url(attempt.url().as_str())
                {
                    attempt.follow()
                } else {
                    attempt.error("redirect target host is not allowed")
                }
            }))
            .build()
            .map_err(ResolveError::HttpClient)?;

        Ok(Self {
            inner: Arc::new(MediaResolverInner {
                providers,
                providers_by_id,
                probe_client,
                metadata_cache_ttl: METADATA_CACHE_TTL,
                metadata_cache: DashMap::new(),
                prepared_cache: DashMap::new(),
                url_aliases: DashMap::new(),
                inflight: DashMap::new(),
                prepare_inflight: DashMap::new(),
                maintenance_tick: AtomicU64::new(0),
                probe_slots: Semaphore::new(MAX_CONCURRENT_PROBES),
            }),
        })
    }

    pub async fn warmup_providers(&self) {
        for provider in &self.inner.providers {
            if let Err(error) = provider.warmup(&self.inner.probe_client).await {
                tracing::debug!(
                    provider_id = provider.id(),
                    error = %error,
                    "media provider warmup failed"
                );
            }
        }
    }

    pub async fn resolve(&self, source_url: &str) -> Result<TrackRequest, ResolveError> {
        if !is_allowed_track_url(source_url) {
            return Err(ResolveError::UnsupportedSource(source_url.to_owned()));
        }

        if let Some(request) = self.lookup_cached_request(source_url) {
            return Ok(request);
        }

        let gate = self
            .inner
            .inflight
            .entry(source_url.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _guard = gate.lock().await;

        if let Some(request) = self.lookup_cached_request(source_url) {
            return Ok(request);
        }

        let _permit = self
            .inner
            .probe_slots
            .acquire()
            .await
            .expect("media probe semaphore should stay open");

        let resolved = self.probe_with_fallback(source_url).await;

        let result = match resolved {
            Ok(request) => match validate_prepared_request(&request) {
                Ok(()) => {
                    self.store_request(source_url, &request);
                    self.store_prepared_request(&request);
                    Ok(request)
                }
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        };
        self.inner.inflight.remove(source_url);
        result
    }

    pub async fn prepare_playback(
        &self,
        request: &TrackRequest,
    ) -> Result<TrackRequest, ResolveError> {
        if let Some(prepared) = self.lookup_prepared_request(request) {
            return Ok(prepared);
        }

        if !should_refresh_on_prepare(request) {
            self.store_prepared_request(request);
            Ok(request.clone())
        } else {
            let gate = self
                .inner
                .prepare_inflight
                .entry(request.canonical_key.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone();
            let _guard = gate.lock().await;

            if let Some(prepared) = self.lookup_prepared_request(request) {
                self.inner
                    .prepare_inflight
                    .remove(request.canonical_key.as_ref());
                return Ok(prepared);
            }

            let _permit = self
                .inner
                .probe_slots
                .acquire()
                .await
                .expect("media probe semaphore should stay open");

            let result = match self.refresh_request(request).await {
                Ok(refreshed) => match validate_prepared_request(&refreshed) {
                    Ok(()) => {
                        self.store_request(request.requested_url.as_ref(), &refreshed);
                        self.store_prepared_request(&refreshed);
                        Ok(refreshed)
                    }
                    Err(error) => Err(error),
                },
                Err(error) => Err(error),
            };
            self.inner
                .prepare_inflight
                .remove(request.canonical_key.as_ref());
            result
        }
    }

    fn lookup_cached_request(&self, source_url: &str) -> Option<TrackRequest> {
        let canonical_key = {
            let alias = self.inner.url_aliases.get(source_url)?;
            if alias.cached_at.elapsed() > self.inner.metadata_cache_ttl {
                drop(alias);
                self.inner.url_aliases.remove(source_url);
                return None;
            }
            alias.canonical_key.clone()
        };

        let cached = self.inner.metadata_cache.get(canonical_key.as_ref())?;
        if cached.cached_at.elapsed() <= self.inner.metadata_cache_ttl {
            let request = cached.request.clone();
            drop(cached);
            if let Some(prepared) = self.lookup_prepared_request(&request) {
                return Some(prepared);
            }
            return Some(request);
        }

        drop(cached);
        self.inner.metadata_cache.remove(canonical_key.as_ref());
        None
    }

    fn store_request(&self, requested_url: &str, request: &TrackRequest) {
        let now = Instant::now();
        self.inner.metadata_cache.insert(
            request.canonical_key.to_string(),
            CachedMetadata {
                request: request.clone(),
                cached_at: now,
            },
        );

        for alias in [
            requested_url,
            request.requested_url.as_ref(),
            request.canonical_url.as_ref(),
        ] {
            self.inner.url_aliases.insert(
                alias.to_owned(),
                CachedAlias {
                    canonical_key: request.canonical_key.clone(),
                    cached_at: now,
                },
            );
        }
        self.maybe_prune_caches();
    }

    fn lookup_prepared_request(&self, request: &TrackRequest) -> Option<TrackRequest> {
        let cached = self
            .inner
            .prepared_cache
            .get(request.canonical_key.as_ref())?;
        if prepared_request_is_fresh(&cached) {
            return Some(cached.request.clone());
        }

        drop(cached);
        self.inner
            .prepared_cache
            .remove(request.canonical_key.as_ref());
        None
    }

    fn store_prepared_request(&self, request: &TrackRequest) {
        self.inner.prepared_cache.insert(
            request.canonical_key.to_string(),
            CachedPrepared {
                request: request.clone(),
                cached_at: Instant::now(),
            },
        );
        self.maybe_prune_caches();
    }

    fn maybe_prune_caches(&self) {
        let tick = self.inner.maintenance_tick.fetch_add(1, Ordering::Relaxed) + 1;
        let over_capacity = self.inner.metadata_cache.len() > MAX_METADATA_CACHE_ITEMS
            || self.inner.prepared_cache.len() > MAX_PREPARED_CACHE_ITEMS
            || self.inner.url_aliases.len() > MAX_URL_ALIAS_ITEMS;
        if !over_capacity && !tick.is_multiple_of(CACHE_MAINTENANCE_INTERVAL) {
            return;
        }

        prune_cached_tracks(
            &self.inner.metadata_cache,
            MAX_METADATA_CACHE_ITEMS,
            |cached| cached.cached_at.elapsed() > self.inner.metadata_cache_ttl,
            |cached| cached.cached_at,
        );
        prune_cached_tracks(
            &self.inner.prepared_cache,
            MAX_PREPARED_CACHE_ITEMS,
            |cached| !prepared_request_is_fresh(cached),
            |cached| cached.cached_at,
        );
        prune_cached_aliases(&self.inner.url_aliases, MAX_URL_ALIAS_ITEMS, |alias| {
            alias.cached_at.elapsed() > self.inner.metadata_cache_ttl
        });
    }

    async fn probe_with_fallback(&self, raw_url: &str) -> Result<TrackRequest, ResolveError> {
        let mut last_error = None;

        for provider in self
            .inner
            .providers
            .iter()
            .filter(|provider| provider.supports(raw_url))
        {
            match provider.probe(raw_url, &self.inner.probe_client).await {
                Ok(request) => return Ok(request),
                Err(error) => last_error = Some(error),
            }
        }

        Err(last_error.unwrap_or_else(|| ResolveError::UnsupportedSource(raw_url.to_owned())))
    }

    async fn refresh_request(&self, request: &TrackRequest) -> Result<TrackRequest, ResolveError> {
        let provider = self.provider_for(request.provider_id.as_ref())?;
        if let Some(refreshed) = provider
            .refresh_playback(request, &self.inner.probe_client)
            .await?
        {
            return Ok(refreshed);
        }

        provider
            .probe(request.requested_url.as_ref(), &self.inner.probe_client)
            .await
    }

    fn provider_for(&self, provider_id: &str) -> Result<Arc<dyn MediaProvider>, ResolveError> {
        self.inner
            .providers_by_id
            .get(provider_id)
            .cloned()
            .ok_or_else(|| ResolveError::MissingProvider(provider_id.to_owned()))
    }
}

fn prepared_source_is_stale(request: &TrackRequest) -> bool {
    let Some(expires_at_unix) = request.prepared.expires_at_unix() else {
        return false;
    };

    expires_at_unix <= unix_now_secs() + PREPARED_REFRESH_SKEW.as_secs()
}

fn should_refresh_on_prepare(request: &TrackRequest) -> bool {
    prepared_source_is_stale(request)
        || refresh_after_prepared_cache_expiry(request.provider_id.as_ref())
}

fn refresh_after_prepared_cache_expiry(provider_id: &str) -> bool {
    matches!(provider_id, "soundcloud" | "niconico" | "x")
}

fn prepared_request_is_fresh(cached: &CachedPrepared) -> bool {
    cached.cached_at.elapsed() <= prepared_cache_ttl(&cached.request)
        && !prepared_source_is_stale(&cached.request)
}

fn prepared_cache_ttl(request: &TrackRequest) -> Duration {
    match request.provider_id.as_ref() {
        "soundcloud" | "niconico" | "x" => TRANSIENT_PREPARED_CACHE_TTL,
        _ => PREPARED_CACHE_TTL,
    }
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Debug, Error)]
pub enum ResolveError {
    #[error("unsupported source: {0}")]
    UnsupportedSource(String),
    #[error("missing media provider: {0}")]
    MissingProvider(String),
    #[error("failed to initialize HTTP client: {0}")]
    HttpClient(reqwest::Error),
    #[error("failed to send HTTP request: {0}")]
    Request(reqwest::Error),
    #[error("failed to parse provider payload: {0}")]
    Parse(String),
    #[error("invalid HTTP header name: {0}")]
    InvalidHeaderName(String),
    #[error("invalid HTTP header value: {0}")]
    InvalidHeaderValue(String),
    #[error("unsafe playback target for provider {provider_id}: {url}")]
    UnsafePlaybackTarget { provider_id: String, url: String },
    #[error("YouTube extraction failed: {0}")]
    YouTube(rusty_ytdl::VideoError),
}

#[async_trait]
impl MediaBackend for MediaResolver {
    type Error = ResolveError;

    async fn resolve(&self, source_url: &str) -> Result<TrackRequest, Self::Error> {
        Self::resolve(self, source_url).await
    }

    async fn prepare_playback(&self, request: &TrackRequest) -> Result<TrackRequest, Self::Error> {
        Self::prepare_playback(self, request).await
    }
}

fn validate_prepared_request(request: &TrackRequest) -> Result<(), ResolveError> {
    let playback_url = match &request.prepared {
        PreparedSource::Http { stream_url, .. } => stream_url.as_ref(),
        PreparedSource::Hls { playlist_url, .. } => playlist_url.as_ref(),
    };

    if is_allowed_prepared_url(request.provider_id.as_ref(), playback_url) {
        Ok(())
    } else {
        Err(ResolveError::UnsafePlaybackTarget {
            provider_id: request.provider_id.to_string(),
            url: summarize_url_for_logs(playback_url),
        })
    }
}

fn prune_cached_tracks<T, F, G>(
    cache: &DashMap<String, T>,
    max_items: usize,
    should_remove: F,
    age_of: G,
) where
    F: Fn(&T) -> bool,
    G: Fn(&T) -> Instant,
{
    let expired: Vec<String> = cache
        .iter()
        .filter(|entry| should_remove(entry.value()))
        .map(|entry| entry.key().clone())
        .collect();
    for key in expired {
        cache.remove(&key);
    }

    trim_dashmap_by_age(cache, max_items, age_of);
}

fn prune_cached_aliases<F>(cache: &DashMap<String, CachedAlias>, max_items: usize, should_remove: F)
where
    F: Fn(&CachedAlias) -> bool,
{
    let expired: Vec<String> = cache
        .iter()
        .filter(|entry| should_remove(entry.value()))
        .map(|entry| entry.key().clone())
        .collect();
    for key in expired {
        cache.remove(&key);
    }

    trim_dashmap_by_age(cache, max_items, |cached| cached.cached_at);
}

fn trim_dashmap_by_age<T, F>(cache: &DashMap<String, T>, max_items: usize, age_of: F)
where
    F: Fn(&T) -> Instant,
{
    let len = cache.len();
    if len <= max_items {
        return;
    }

    let mut entries: Vec<(String, Instant)> = cache
        .iter()
        .map(|entry| (entry.key().clone(), age_of(entry.value())))
        .collect();
    entries.sort_by_key(|(_, cached_at)| *cached_at);

    let remove_count = len.saturating_sub(max_items);
    for (key, _) in entries.into_iter().take(remove_count) {
        cache.remove(&key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wotoha_core::TrackMetadata;

    fn sample_request(expires_at_unix: Option<u64>) -> TrackRequest {
        TrackRequest::new(
            "youtube",
            "youtube:video:dQw4w9WgXcQ",
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://rr1---sn.example.googlevideo.com/videoplayback?id=123",
            PreparedSource::http(
                "https://rr1---sn.example.googlevideo.com/videoplayback?id=123",
                Vec::new(),
                Some(2_891_031),
                expires_at_unix,
            ),
            TrackMetadata::new(
                "Never Gonna Give You Up",
                "Rick Astley",
                "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
                None,
                Some(Duration::from_secs(213)),
            ),
        )
    }

    #[test]
    fn metadata_cache_survives_stale_prepared_sources() {
        let resolver = MediaResolver::new().expect("resolver");
        let stale_request = sample_request(Some(unix_now_secs().saturating_sub(5)));
        resolver.store_request(stale_request.requested_url.as_ref(), &stale_request);

        let cached = resolver
            .lookup_cached_request(stale_request.requested_url.as_ref())
            .expect("cached metadata request");

        assert_eq!(cached.canonical_key, stale_request.canonical_key);
    }

    #[test]
    fn metadata_lookup_prefers_fresh_prepared_cache() {
        let resolver = MediaResolver::new().expect("resolver");
        let stale_request = sample_request(Some(unix_now_secs().saturating_sub(5)));
        let fresh_request = sample_request(Some(unix_now_secs().saturating_add(600)));

        resolver.store_request(stale_request.requested_url.as_ref(), &stale_request);
        resolver.store_prepared_request(&fresh_request);

        let cached = resolver
            .lookup_cached_request(stale_request.requested_url.as_ref())
            .expect("merged cached request");

        assert_eq!(
            cached.prepared.expires_at_unix(),
            fresh_request.prepared.expires_at_unix()
        );
    }

    #[test]
    fn transient_sources_refresh_when_prepared_cache_expired() {
        let mut request = sample_request(None);
        request.provider_id = "soundcloud".into();

        assert!(should_refresh_on_prepare(&request));

        request.provider_id = "youtube".into();
        assert!(!should_refresh_on_prepare(&request));
    }
}
