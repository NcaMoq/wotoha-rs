use async_trait::async_trait;
use reqwest::{Client, Url};
use wotoha_core::TrackRequest;

use crate::{ResolveError, provider::MediaProvider};

use super::youtube_ytdlp;

/// YouTube extraction is deliberately delegated to yt-dlp. The resolver owns
/// caching and stream-target validation; this provider owns URL routing and
/// refreshes short-lived playback URLs.
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
        matches!(url.host_str().map(|host| host.to_ascii_lowercase()), Some(host) if matches!(host.as_str(), "youtube.com" | "www.youtube.com" | "m.youtube.com" | "music.youtube.com" | "youtu.be"))
    }

    async fn probe(
        &self,
        raw_url: &str,
        _probe_client: &Client,
    ) -> Result<TrackRequest, ResolveError> {
        youtube_ytdlp::probe(raw_url)
            .await
            .map_err(ResolveError::YouTube)
    }

    async fn refresh_playback(
        &self,
        request: &TrackRequest,
        _probe_client: &Client,
    ) -> Result<Option<TrackRequest>, ResolveError> {
        youtube_ytdlp::refresh(request)
            .await
            .map(Some)
            .map_err(ResolveError::YouTube)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn supports_common_youtube_hosts() {
        let provider = YouTubeProvider;
        assert!(provider.supports("https://www.youtube.com/watch?v=dQw4w9WgXcQ"));
        assert!(provider.supports("https://youtu.be/dQw4w9WgXcQ"));
        assert!(!provider.supports("https://example.com/watch?v=dQw4w9WgXcQ"));
    }
}
