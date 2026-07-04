use async_trait::async_trait;
use reqwest::Client;
use wotoha_core::TrackRequest;

use crate::ResolveError;

#[async_trait]
pub trait MediaProvider: Send + Sync {
    fn id(&self) -> &'static str;
    fn supports(&self, raw_url: &str) -> bool;

    async fn warmup(&self, probe_client: &Client) -> Result<(), ResolveError> {
        let _ = probe_client;
        Ok(())
    }

    async fn probe(
        &self,
        raw_url: &str,
        probe_client: &Client,
    ) -> Result<TrackRequest, ResolveError>;

    async fn refresh_playback(
        &self,
        request: &TrackRequest,
        probe_client: &Client,
    ) -> Result<Option<TrackRequest>, ResolveError> {
        let _ = (request, probe_client);
        Ok(None)
    }
}
