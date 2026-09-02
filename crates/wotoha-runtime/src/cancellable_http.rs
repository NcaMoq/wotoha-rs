use std::{
    future::Future,
    io::{Error as IoError, ErrorKind as IoErrorKind, Result as IoResult, SeekFrom},
    pin::Pin,
    task::{Context, Poll},
};

use async_trait::async_trait;
use futures::TryStreamExt;
use pin_project::pin_project;
use reqwest::{Client, header::HeaderMap};
use songbird::input::{
    AsyncAdapterStream, AsyncMediaSource, AudioStream, AudioStreamError, Compose, Input,
    core::io::MediaSource,
};
use tokio::io::{AsyncRead, AsyncSeek, ReadBuf};
use tokio_util::{
    io::StreamReader,
    sync::{CancellationToken, WaitForCancellationFutureOwned},
};

/// A direct HTTP input whose response body can be interrupted when its owner
/// is cancelled. Songbird's built-in `HttpRequest` has no cancellation hook;
/// this keeps the same lazy/async adapter behavior while adding one.
#[derive(Clone, Debug)]
pub(crate) struct CancellableHttpRequest {
    pub(crate) client: Client,
    pub(crate) request: String,
    pub(crate) headers: HeaderMap,
    pub(crate) content_length: Option<u64>,
    cancellation: Option<CancellationToken>,
}

impl CancellableHttpRequest {
    pub(crate) fn new_with_headers(
        client: Client,
        request: String,
        headers: HeaderMap,
        content_length: Option<u64>,
        cancellation: Option<CancellationToken>,
    ) -> Self {
        Self {
            client,
            request,
            headers,
            content_length,
            cancellation,
        }
    }

    async fn create_stream(&mut self) -> Result<CancellableHttpStream, AudioStreamError> {
        let mut request = self.client.get(&self.request).headers(self.headers.clone());
        if let Some(content_length) = self.content_length {
            request = request.header(
                reqwest::header::RANGE,
                format!("bytes=0-{}", content_length.saturating_sub(1)),
            );
        }

        let response = if let Some(cancellation) = self.cancellation.clone() {
            tokio::select! {
                _ = cancellation.cancelled() => return Err(cancelled_stream_error()),
                response = request.send() => response
                    .map_err(|error| AudioStreamError::Fail(Box::new(error)))?,
            }
        } else {
            request
                .send()
                .await
                .map_err(|error| AudioStreamError::Fail(Box::new(error)))?
        };
        if !response.status().is_success() {
            return Err(AudioStreamError::Fail(
                format!("failed with http status code: {}", response.status()).into(),
            ));
        }

        let length = response
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse().ok());
        let stream = Box::new(StreamReader::new(
            response.bytes_stream().map_err(IoError::other),
        ));
        Ok(CancellableHttpStream {
            stream,
            length,
            cancellation: self.cancellation.clone(),
            cancellation_wait: self
                .cancellation
                .clone()
                .map(|token| Box::pin(token.cancelled_owned())),
        })
    }
}

#[pin_project]
struct CancellableHttpStream {
    #[pin]
    stream: Box<dyn AsyncRead + Send + Sync + Unpin>,
    length: Option<u64>,
    cancellation: Option<CancellationToken>,
    cancellation_wait: Option<Pin<Box<WaitForCancellationFutureOwned>>>,
}

impl AsyncRead for CancellableHttpStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<IoResult<()>> {
        let mut this = self.project();
        if this
            .cancellation
            .as_ref()
            .is_some_and(|cancellation| cancellation.is_cancelled())
            || this
                .cancellation_wait
                .as_mut()
                .is_some_and(|wait| wait.as_mut().poll(cx).is_ready())
        {
            return Poll::Ready(Err(IoError::new(
                IoErrorKind::Interrupted,
                "stream cancelled",
            )));
        }
        this.stream.as_mut().poll_read(cx, buf)
    }
}

impl AsyncSeek for CancellableHttpStream {
    fn start_seek(self: Pin<&mut Self>, _position: SeekFrom) -> IoResult<()> {
        Err(IoErrorKind::Unsupported.into())
    }

    fn poll_complete(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<IoResult<u64>> {
        unreachable!()
    }
}

#[async_trait]
impl AsyncMediaSource for CancellableHttpStream {
    fn is_seekable(&self) -> bool {
        false
    }

    async fn byte_len(&self) -> Option<u64> {
        self.length
    }
}

#[async_trait]
impl Compose for CancellableHttpRequest {
    fn create(&mut self) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
        Err(AudioStreamError::Unsupported)
    }

    async fn create_async(
        &mut self,
    ) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
        self.create_stream().await.map(|stream| {
            let stream = AsyncAdapterStream::new(Box::new(stream), 64 * 1024);
            AudioStream {
                input: Box::new(stream) as Box<dyn MediaSource>,
            }
        })
    }

    fn should_create_async(&self) -> bool {
        true
    }
}

impl From<CancellableHttpRequest> for Input {
    fn from(value: CancellableHttpRequest) -> Self {
        Input::Lazy(Box::new(value))
    }
}

fn cancelled_stream_error() -> AudioStreamError {
    AudioStreamError::Fail(Box::new(IoError::new(
        IoErrorKind::Interrupted,
        "stream cancelled",
    )))
}
