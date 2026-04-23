use async_trait::async_trait;
use reqwest::Client;
use songbird::input::Input;
use wotoha_core::TrackRequest;

use crate::ResolveError;

#[async_trait]
pub trait MediaProvider: Send + Sync {
    fn id(&self) -> &'static str;
    fn supports(&self, raw_url: &str) -> bool;

    async fn probe(
        &self,
        raw_url: &str,
        probe_client: &Client,
    ) -> Result<TrackRequest, ResolveError>;

    fn open_input(
        &self,
        request: &TrackRequest,
        stream_client: &Client,
    ) -> Result<Input, ResolveError>;
}
