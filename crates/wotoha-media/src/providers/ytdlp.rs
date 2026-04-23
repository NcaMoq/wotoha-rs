use std::sync::Arc;

use async_trait::async_trait;
use reqwest::Client;
use songbird::input::{Input, YoutubeDl};
use wotoha_core::{TrackMetadata, TrackRequest};

use crate::{ResolveError, provider::MediaProvider};

#[derive(Clone, Debug, Default)]
pub struct YtDlpProvider;

#[async_trait]
impl MediaProvider for YtDlpProvider {
    fn id(&self) -> &'static str {
        "ytdlp"
    }

    fn supports(&self, _raw_url: &str) -> bool {
        true
    }

    async fn probe(
        &self,
        raw_url: &str,
        probe_client: &Client,
    ) -> Result<TrackRequest, ResolveError> {
        let mut input: Input = YoutubeDl::new(probe_client.clone(), raw_url.to_owned()).into();
        let aux = input.aux_metadata().await.map_err(ResolveError::Metadata)?;

        Ok(TrackRequest::new(
            self.id(),
            raw_url.to_owned(),
            TrackMetadata::new(
                aux.track
                    .or(aux.title)
                    .unwrap_or_else(|| raw_url.to_owned()),
                aux.artist
                    .or(aux.channel)
                    .unwrap_or_else(|| "Unknown".to_owned()),
                aux.source_url.unwrap_or_else(|| raw_url.to_owned()),
                aux.thumbnail.map(Arc::<str>::from),
                aux.duration,
            ),
        ))
    }

    fn open_input(
        &self,
        request: &TrackRequest,
        stream_client: &Client,
    ) -> Result<Input, ResolveError> {
        Ok(YoutubeDl::new(stream_client.clone(), request.source_url.to_string()).into())
    }
}
