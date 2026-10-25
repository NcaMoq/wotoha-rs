use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

const MAX_TRACK_DURATION: Duration = Duration::from_secs(24 * 60 * 60);
static NEXT_TRACK_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TrackRequestId(u64);

impl TrackRequestId {
    fn fresh() -> Self {
        Self(NEXT_TRACK_REQUEST_ID.fetch_add(1, Ordering::Relaxed))
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TrackMetadata {
    pub title: Arc<str>,
    pub author: Arc<str>,
    pub uri: Arc<str>,
    pub thumbnail_url: Option<Arc<str>>,
    pub duration: Option<Duration>,
}

impl TrackMetadata {
    pub fn new(
        title: impl Into<Arc<str>>,
        author: impl Into<Arc<str>>,
        uri: impl Into<Arc<str>>,
        thumbnail_url: Option<Arc<str>>,
        duration: Option<Duration>,
    ) -> Self {
        let duration =
            duration.filter(|duration| !duration.is_zero() && *duration <= MAX_TRACK_DURATION);
        Self {
            title: title.into(),
            author: author.into(),
            uri: uri.into(),
            thumbnail_url,
            duration,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedHeader {
    pub name: Arc<str>,
    pub value: Arc<str>,
}

impl PreparedHeader {
    pub fn new(name: impl Into<Arc<str>>, value: impl Into<Arc<str>>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Default)]
pub enum PreparedRangeMode {
    #[default]
    Header,
    QueryParam,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PreparedSource {
    Http {
        stream_url: Arc<str>,
        headers: Arc<[PreparedHeader]>,
        content_length: Option<u64>,
        range_chunk_size: Option<u64>,
        range_mode: PreparedRangeMode,
        expires_at_unix: Option<u64>,
    },
    Hls {
        playlist_url: Arc<str>,
        headers: Arc<[PreparedHeader]>,
        expires_at_unix: Option<u64>,
    },
}

impl PreparedSource {
    pub fn http(
        stream_url: impl Into<Arc<str>>,
        headers: impl Into<Arc<[PreparedHeader]>>,
        content_length: Option<u64>,
        expires_at_unix: Option<u64>,
    ) -> Self {
        Self::http_with_range(stream_url, headers, content_length, None, expires_at_unix)
    }

    pub fn http_with_range(
        stream_url: impl Into<Arc<str>>,
        headers: impl Into<Arc<[PreparedHeader]>>,
        content_length: Option<u64>,
        range_chunk_size: Option<u64>,
        expires_at_unix: Option<u64>,
    ) -> Self {
        Self::http_with_range_mode(
            stream_url,
            headers,
            content_length,
            range_chunk_size,
            PreparedRangeMode::Header,
            expires_at_unix,
        )
    }

    pub fn http_with_range_mode(
        stream_url: impl Into<Arc<str>>,
        headers: impl Into<Arc<[PreparedHeader]>>,
        content_length: Option<u64>,
        range_chunk_size: Option<u64>,
        range_mode: PreparedRangeMode,
        expires_at_unix: Option<u64>,
    ) -> Self {
        Self::Http {
            stream_url: stream_url.into(),
            headers: headers.into(),
            content_length,
            range_chunk_size,
            range_mode,
            expires_at_unix,
        }
    }

    pub fn hls(
        playlist_url: impl Into<Arc<str>>,
        headers: impl Into<Arc<[PreparedHeader]>>,
        expires_at_unix: Option<u64>,
    ) -> Self {
        Self::Hls {
            playlist_url: playlist_url.into(),
            headers: headers.into(),
            expires_at_unix,
        }
    }

    pub fn expires_at_unix(&self) -> Option<u64> {
        match self {
            Self::Http {
                expires_at_unix, ..
            }
            | Self::Hls {
                expires_at_unix, ..
            } => *expires_at_unix,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrackRequest {
    pub request_id: TrackRequestId,
    pub provider_id: Arc<str>,
    pub canonical_key: Arc<str>,
    pub requested_url: Arc<str>,
    pub canonical_url: Arc<str>,
    pub source_url: Arc<str>,
    pub prepared: PreparedSource,
    pub metadata: TrackMetadata,
}

impl TrackRequest {
    pub fn new(
        provider_id: impl Into<Arc<str>>,
        canonical_key: impl Into<Arc<str>>,
        requested_url: impl Into<Arc<str>>,
        canonical_url: impl Into<Arc<str>>,
        source_url: impl Into<Arc<str>>,
        prepared: PreparedSource,
        metadata: TrackMetadata,
    ) -> Self {
        Self {
            request_id: TrackRequestId::fresh(),
            provider_id: provider_id.into(),
            canonical_key: canonical_key.into(),
            requested_url: requested_url.into(),
            canonical_url: canonical_url.into(),
            source_url: source_url.into(),
            prepared,
            metadata,
        }
    }

    /// Assign a new identity for a separately queued instance of this source.
    pub fn with_fresh_request_id(mut self) -> Self {
        self.request_id = TrackRequestId::fresh();
        self
    }

    /// Preserve queue-entry identity when refreshing only the prepared source.
    pub fn with_request_id(mut self, request_id: TrackRequestId) -> Self {
        self.request_id = request_id;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::{PreparedSource, TrackMetadata, TrackRequest};
    use std::time::Duration;

    #[test]
    fn rejects_unbounded_external_duration_metadata() {
        assert_eq!(
            TrackMetadata::new(
                "title",
                "author",
                "https://example.com",
                None,
                Some(Duration::ZERO)
            )
            .duration,
            None
        );
        assert_eq!(
            TrackMetadata::new(
                "title",
                "author",
                "https://example.com",
                None,
                Some(Duration::from_secs(24 * 60 * 60 + 1)),
            )
            .duration,
            None
        );
        assert_eq!(
            TrackMetadata::new(
                "title",
                "author",
                "https://example.com",
                None,
                Some(Duration::from_secs(60)),
            )
            .duration,
            Some(Duration::from_secs(60))
        );
    }

    #[test]
    fn track_request_identity_is_unique_per_queue_entry_and_stable_on_clone() {
        let request = TrackRequest::new(
            "test",
            "same-track",
            "https://example.com/request",
            "https://example.com/canonical",
            "https://example.com/source",
            PreparedSource::http("https://example.com/stream", Vec::new(), None, None),
            TrackMetadata::new("title", "artist", "https://example.com", None, None),
        );
        let requeued = request.clone().with_fresh_request_id();

        assert_eq!(request.request_id, request.clone().request_id);
        assert_ne!(request.request_id, requeued.request_id);
        assert_eq!(
            request.request_id,
            requeued.with_request_id(request.request_id).request_id
        );
    }
}
