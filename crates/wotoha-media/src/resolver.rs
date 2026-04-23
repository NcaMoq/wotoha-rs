use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use dashmap::DashMap;
use reqwest::{Client, redirect::Policy};
use songbird::input::Input;
use thiserror::Error;
use tokio::sync::{Mutex, Semaphore};
use wotoha_contracts::MediaBackend;
use wotoha_core::TrackRequest;

use crate::{provider::MediaProvider, providers::YtDlpProvider};

const PROBE_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const PROBE_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const PROBE_POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(15);
const STREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const STREAM_TCP_KEEPALIVE: Duration = Duration::from_secs(30);
const STREAM_POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const METADATA_CACHE_TTL: Duration = Duration::from_secs(30 * 60);
const MAX_CONCURRENT_PROBES: usize = 8;

#[derive(Clone)]
pub struct MediaResolver {
    inner: Arc<MediaResolverInner>,
}

struct MediaResolverInner {
    providers: Vec<Arc<dyn MediaProvider>>,
    probe_client: Client,
    stream_client: Client,
    metadata_cache_ttl: Duration,
    metadata_cache: DashMap<String, CachedTrack>,
    inflight: DashMap<String, Arc<Mutex<()>>>,
    probe_slots: Semaphore,
}

#[derive(Clone)]
struct CachedTrack {
    request: TrackRequest,
    cached_at: Instant,
}

impl MediaResolver {
    pub fn new() -> Result<Self, ResolveError> {
        let probe_client = Client::builder()
            .user_agent("wotoha-rust/0.1.0")
            .connect_timeout(PROBE_CONNECT_TIMEOUT)
            .timeout(PROBE_REQUEST_TIMEOUT)
            .pool_idle_timeout(PROBE_POOL_IDLE_TIMEOUT)
            .pool_max_idle_per_host(8)
            .redirect(Policy::limited(5))
            .build()
            .map_err(ResolveError::HttpClient)?;

        let stream_client = Client::builder()
            .user_agent("wotoha-rust/0.1.0")
            .connect_timeout(STREAM_CONNECT_TIMEOUT)
            .tcp_keepalive(STREAM_TCP_KEEPALIVE)
            .pool_idle_timeout(STREAM_POOL_IDLE_TIMEOUT)
            .pool_max_idle_per_host(32)
            .redirect(Policy::limited(5))
            .build()
            .map_err(ResolveError::HttpClient)?;

        Ok(Self {
            inner: Arc::new(MediaResolverInner {
                providers: vec![Arc::new(YtDlpProvider)],
                probe_client,
                stream_client,
                metadata_cache_ttl: METADATA_CACHE_TTL,
                metadata_cache: DashMap::new(),
                inflight: DashMap::new(),
                probe_slots: Semaphore::new(MAX_CONCURRENT_PROBES),
            }),
        })
    }

    pub async fn resolve(&self, source_url: &str) -> Result<TrackRequest, ResolveError> {
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

        let provider = self.provider_for(source_url)?;
        let resolved = provider.probe(source_url, &self.inner.probe_client).await;

        match resolved {
            Ok(request) => {
                self.inner.metadata_cache.insert(
                    source_url.to_owned(),
                    CachedTrack {
                        request: request.clone(),
                        cached_at: Instant::now(),
                    },
                );
                self.inner.inflight.remove(source_url);
                Ok(request)
            }
            Err(error) => {
                self.inner.inflight.remove(source_url);
                Err(error)
            }
        }
    }

    pub fn open_input(&self, request: &TrackRequest) -> Result<Input, ResolveError> {
        self.provider_by_id(request.provider_id.as_ref())
            .ok_or_else(|| ResolveError::UnsupportedProvider(request.provider_id.to_string()))?
            .open_input(request, &self.inner.stream_client)
    }

    fn lookup_cached_request(&self, source_url: &str) -> Option<TrackRequest> {
        let cached = self.inner.metadata_cache.get(source_url)?;

        if cached.cached_at.elapsed() <= self.inner.metadata_cache_ttl {
            return Some(cached.request.clone());
        }

        drop(cached);
        self.inner.metadata_cache.remove(source_url);
        None
    }

    fn provider_for(&self, raw_url: &str) -> Result<Arc<dyn MediaProvider>, ResolveError> {
        self.inner
            .providers
            .iter()
            .find(|provider| provider.supports(raw_url))
            .cloned()
            .ok_or_else(|| ResolveError::UnsupportedSource(raw_url.to_owned()))
    }

    fn provider_by_id(&self, provider_id: &str) -> Option<Arc<dyn MediaProvider>> {
        self.inner
            .providers
            .iter()
            .find(|provider| provider.id() == provider_id)
            .cloned()
    }
}

#[derive(Debug, Error)]
pub enum ResolveError {
    #[error("unsupported source: {0}")]
    UnsupportedSource(String),
    #[error("unsupported provider: {0}")]
    UnsupportedProvider(String),
    #[error("failed to initialize HTTP client: {0}")]
    HttpClient(reqwest::Error),
    #[error("failed to resolve media metadata: {0}")]
    Metadata(songbird::input::AuxMetadataError),
}

#[async_trait]
impl MediaBackend for MediaResolver {
    type Error = ResolveError;

    async fn resolve(&self, source_url: &str) -> Result<TrackRequest, Self::Error> {
        Self::resolve(self, source_url).await
    }

    fn open_input(&self, request: &TrackRequest) -> Result<Input, Self::Error> {
        Self::open_input(self, request)
    }
}
