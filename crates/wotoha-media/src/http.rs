use std::time::Duration;

use futures::StreamExt;
use reqwest::Response;
use serde::de::DeserializeOwned;

use crate::ResolveError;

pub(crate) const MAX_PROVIDER_BODY_BYTES: usize = 8 * 1024 * 1024;
const PROVIDER_BODY_DEADLINE: Duration = Duration::from_secs(20);

pub(crate) async fn bounded_text(response: Response) -> Result<String, ResolveError> {
    let bytes = bounded_bytes(response).await?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

pub(crate) async fn bounded_json<T: DeserializeOwned>(
    response: Response,
) -> Result<T, ResolveError> {
    let bytes = bounded_bytes(response).await?;
    serde_json::from_slice(&bytes).map_err(|error| ResolveError::Parse(error.to_string()))
}

async fn bounded_bytes(response: Response) -> Result<Vec<u8>, ResolveError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_PROVIDER_BODY_BYTES as u64)
    {
        return Err(ResolveError::BodyTooLarge {
            limit: MAX_PROVIDER_BODY_BYTES,
        });
    }

    tokio::time::timeout(PROVIDER_BODY_DEADLINE, async move {
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(ResolveError::Request)?;
            if body.len().saturating_add(chunk.len()) > MAX_PROVIDER_BODY_BYTES {
                return Err(ResolveError::BodyTooLarge {
                    limit: MAX_PROVIDER_BODY_BYTES,
                });
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    })
    .await
    .map_err(|_| ResolveError::BodyTimeout(PROVIDER_BODY_DEADLINE))?
}
