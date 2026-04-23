use std::{sync::Arc, time::Duration};

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
pub struct TrackRequest {
    pub provider_id: Arc<str>,
    pub source_url: Arc<str>,
    pub metadata: TrackMetadata,
}

impl TrackRequest {
    pub fn new(
        provider_id: impl Into<Arc<str>>,
        source_url: impl Into<Arc<str>>,
        metadata: TrackMetadata,
    ) -> Self {
        Self {
            provider_id: provider_id.into(),
            source_url: source_url.into(),
            metadata,
        }
    }
}
