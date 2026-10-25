use std::{collections::VecDeque, time::Duration};

use async_trait::async_trait;
use futures::StreamExt;
use hls_m3u8::{
    Decryptable, MasterPlaylist, MediaPlaylist,
    tags::{ExtXMap, VariantStream},
};
use patricia_tree::PatriciaSet;
use reqwest::{Client, Url, header::HeaderMap};
use songbird::input::{
    AsyncAdapterStream, AsyncMediaSource, AudioStream, AudioStreamError, Compose, Input,
    core::io::MediaSource,
};
use tokio::{
    io::{AsyncRead, AsyncSeek, AsyncWriteExt, DuplexStream, ReadBuf, duplex},
    sync::{
        mpsc::{Receiver, Sender, channel},
        watch,
    },
};
use tokio_util::sync::CancellationToken;
use url::ParseError;

use crate::hls_security::{filtered_headers, validate_provider_url};

const HLS_MAX_RETRIES: usize = 12;
const PLAYLIST_TIMEOUT: Duration = Duration::from_secs(10);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(10);
const HLS_MIN_PLAYLIST_RELOAD: Duration = Duration::from_millis(500);
const HLS_MAX_PLAYLIST_RELOAD: Duration = Duration::from_secs(15);
const HLS_SEGMENT_RETRY_DELAY: Duration = Duration::from_millis(150);
const HLS_SEGMENT_DOWNLOAD_ATTEMPTS: usize = 2;
const SEGMENT_QUEUE_CAPACITY: usize = 16;
const SEGMENT_LINK_CAPACITY: usize = 512;
const STREAM_BUFFER_BYTES: usize = 64 * 1024;
const MAX_PLAYLIST_BODY_BYTES: usize = 8 * 1024 * 1024;
const MAX_HLS_SEGMENT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct ValidatedHlsRequest {
    client: Client,
    provider_id: String,
    playlist_url: String,
    headers: HeaderMap,
    cancellation: Option<CancellationToken>,
}

#[derive(Clone, Debug)]
struct SegmentRequest {
    url: Url,
    headers: HeaderMap,
}

enum SegmentQueue {
    Url(SegmentRequest),
    StreamOver,
}

impl ValidatedHlsRequest {
    pub fn new(
        client: Client,
        provider_id: impl Into<String>,
        playlist_url: String,
        headers: HeaderMap,
    ) -> Self {
        Self::new_with_cancellation(client, provider_id, playlist_url, headers, None)
    }

    pub(crate) fn new_with_cancellation(
        client: Client,
        provider_id: impl Into<String>,
        playlist_url: String,
        headers: HeaderMap,
        cancellation: Option<CancellationToken>,
    ) -> Self {
        Self {
            client,
            provider_id: provider_id.into(),
            playlist_url,
            headers,
            cancellation,
        }
    }

    fn create_stream(&mut self) -> Result<ValidatedHlsAsyncSource, AudioStreamError> {
        validate_provider_url(&self.provider_id, &self.playlist_url)?;

        let (segment_tx, segment_rx) = channel(SEGMENT_QUEUE_CAPACITY);
        let (stop_tx, stop_rx) = watch::channel(false);
        let (writer, reader) = duplex(STREAM_BUFFER_BYTES);
        let watcher = HlsWatcher::new(
            self.client.clone(),
            self.provider_id.clone(),
            self.playlist_url.clone(),
            self.headers.clone(),
        )?;
        let downloader_client = self.client.clone();
        let watcher_tx = segment_tx.clone();
        let watcher_stop_rx = stop_rx.clone();
        let watcher_stop_check = stop_rx.clone();
        tokio::spawn(async move {
            if let Err(error) = watcher.run(watcher_tx.clone(), watcher_stop_rx).await
                && !stop_requested(&watcher_stop_check)
            {
                tracing::warn!("validated hls watcher failed: {error}");
                let _ = watcher_tx.send(SegmentQueue::StreamOver).await;
            }
        });
        let forwarder_stop_tx = stop_tx.clone();
        tokio::spawn(async move {
            bytes_forwarder(
                downloader_client,
                segment_rx,
                writer,
                forwarder_stop_tx,
                stop_rx,
            )
            .await;
        });
        if let Some(cancellation) = self.cancellation.clone() {
            let stop_tx = stop_tx.clone();
            tokio::spawn(async move {
                cancellation.cancelled().await;
                request_stop(&stop_tx);
            });
        }

        Ok(ValidatedHlsAsyncSource {
            stream: reader,
            stop_tx,
        })
    }
}

async fn bytes_forwarder(
    client: Client,
    mut segment_rx: Receiver<SegmentQueue>,
    mut writer: DuplexStream,
    stop_tx: watch::Sender<bool>,
    mut stop_rx: watch::Receiver<bool>,
) {
    loop {
        let item = tokio::select! {
            item = segment_rx.recv() => item,
            changed = stop_rx.changed() => {
                if changed.is_err() || stop_requested(&stop_rx) {
                    break;
                }
                continue;
            }
        };

        let Some(item) = item else {
            break;
        };

        match item {
            SegmentQueue::Url(segment) => {
                match download_segment_with_retry(
                    client.clone(),
                    segment,
                    &mut writer,
                    &mut stop_rx,
                )
                .await
                {
                    Ok(SegmentDownload::Completed) => {}
                    Ok(SegmentDownload::Stopped) => break,
                    Err(error) => {
                        tracing::warn!(
                            reason = error.reason(),
                            "validated hls segment download failed after retry"
                        );
                        break;
                    }
                }
            }
            SegmentQueue::StreamOver => {
                break;
            }
        }
    }

    request_stop(&stop_tx);
    let _ = writer.shutdown().await;
}

enum SegmentDownload {
    Completed,
    Stopped,
}

enum SegmentDownloadFailure {
    Request(reqwest::Error),
    HttpStatus(reqwest::StatusCode),
    Body(reqwest::Error),
    BodyTimeout,
    TooLarge,
    Writer,
}

impl SegmentDownloadFailure {
    fn retryable(&self) -> bool {
        match self {
            Self::Request(error) | Self::Body(error) => {
                error.is_timeout() || error.is_connect() || error.is_body() || error.is_decode()
            }
            Self::HttpStatus(status) => status.as_u16() == 429 || status.is_server_error(),
            Self::BodyTimeout => true,
            Self::TooLarge | Self::Writer => false,
        }
    }

    fn reason(&self) -> &'static str {
        match self {
            Self::Request(error) if error.is_timeout() => "request_timeout",
            Self::Request(error) if error.is_connect() => "connect_error",
            Self::Request(_) => "request_error",
            Self::HttpStatus(status) if status.as_u16() == 429 => "http_429",
            Self::HttpStatus(status) if status.is_server_error() => "http_5xx",
            Self::HttpStatus(_) => "http_status",
            Self::Body(error) if error.is_decode() => "body_decode_error",
            Self::Body(error) if error.is_timeout() => "body_timeout",
            Self::Body(_) => "body_error",
            Self::BodyTimeout => "body_timeout",
            Self::TooLarge => "segment_too_large",
            Self::Writer => "audio_pipe_write_error",
        }
    }
}

async fn download_segment_with_retry(
    client: Client,
    segment: SegmentRequest,
    writer: &mut DuplexStream,
    stop_rx: &mut watch::Receiver<bool>,
) -> Result<SegmentDownload, SegmentDownloadFailure> {
    for attempt in 1..=HLS_SEGMENT_DOWNLOAD_ATTEMPTS {
        match download_segment(client.clone(), &segment, writer, stop_rx).await {
            Ok(result) => return Ok(result),
            Err(error) if attempt < HLS_SEGMENT_DOWNLOAD_ATTEMPTS && error.retryable() => {
                tracing::warn!(
                    attempt,
                    reason = error.reason(),
                    "retrying transient hls segment failure"
                );
                if sleep_or_stop(HLS_SEGMENT_RETRY_DELAY, stop_rx).await {
                    return Ok(SegmentDownload::Stopped);
                }
            }
            Err(error) => return Err(error),
        }
    }

    unreachable!("the segment download attempt count is nonzero")
}

async fn download_segment(
    client: Client,
    segment: &SegmentRequest,
    writer: &mut DuplexStream,
    stop_rx: &mut watch::Receiver<bool>,
) -> Result<SegmentDownload, SegmentDownloadFailure> {
    if stop_requested(stop_rx) {
        return Ok(SegmentDownload::Stopped);
    }

    let request = client
        .get(segment.url.clone())
        .headers(segment.headers.clone())
        .timeout(DOWNLOAD_TIMEOUT)
        .build()
        .map_err(SegmentDownloadFailure::Request)?;

    let response = tokio::select! {
        response = client.execute(request) => response.map_err(SegmentDownloadFailure::Request)?,
        changed = stop_rx.changed() => {
            if changed.is_err() || stop_requested(stop_rx) {
                return Ok(SegmentDownload::Stopped);
            }
            return Err(SegmentDownloadFailure::BodyTimeout);
        }
    };

    let status = response.status();
    if !status.is_success() {
        return Err(SegmentDownloadFailure::HttpStatus(status));
    }

    if response
        .content_length()
        .is_some_and(|length| length > MAX_HLS_SEGMENT_BYTES as u64)
    {
        return Err(SegmentDownloadFailure::TooLarge);
    }

    let mut body = Vec::with_capacity(
        response
            .content_length()
            .unwrap_or_default()
            .min(MAX_HLS_SEGMENT_BYTES as u64) as usize,
    );
    let mut stream = response.bytes_stream();

    loop {
        if stop_requested(stop_rx) {
            return Ok(SegmentDownload::Stopped);
        }

        let next = tokio::select! {
            next = tokio::time::timeout(DOWNLOAD_TIMEOUT, stream.next()) => {
                next.map_err(|_| SegmentDownloadFailure::BodyTimeout)?
            },
            changed = stop_rx.changed() => {
                if changed.is_err() || stop_requested(stop_rx) {
                    return Ok(SegmentDownload::Stopped);
                }
                continue;
            }
        };

        match next {
            Some(Ok(bytes)) => {
                if body.len().saturating_add(bytes.len()) > MAX_HLS_SEGMENT_BYTES {
                    return Err(SegmentDownloadFailure::TooLarge);
                }
                body.extend_from_slice(&bytes);
            }
            Some(Err(error)) => return Err(SegmentDownloadFailure::Body(error)),
            None => break,
        }
    }

    let write_result = loop {
        tokio::select! {
            result = writer.write_all(&body) => break result,
            changed = stop_rx.changed() => {
                if changed.is_err() || stop_requested(stop_rx) {
                    return Ok(SegmentDownload::Stopped);
                }
            }
        }
    };
    write_result.map_err(|_| SegmentDownloadFailure::Writer)?;

    Ok(SegmentDownload::Completed)
}

struct HlsWatcher {
    client: Client,
    provider_id: String,
    playlist_url: Url,
    headers: HeaderMap,
    links: RecentSegmentLinks,
    active_init_map: Option<String>,
    timeout: Duration,
    fail_counter: usize,
}

impl HlsWatcher {
    fn new(
        client: Client,
        provider_id: String,
        playlist_url: String,
        headers: HeaderMap,
    ) -> Result<Self, AudioStreamError> {
        let playlist_url =
            Url::parse(&playlist_url).map_err(|error| AudioStreamError::Fail(Box::new(error)))?;
        Ok(Self {
            client,
            provider_id,
            playlist_url,
            headers,
            links: RecentSegmentLinks::new(SEGMENT_LINK_CAPACITY),
            active_init_map: None,
            timeout: PLAYLIST_TIMEOUT,
            fail_counter: 0,
        })
    }

    async fn run(
        mut self,
        queue_tx: Sender<SegmentQueue>,
        mut stop_rx: watch::Receiver<bool>,
    ) -> Result<(), String> {
        loop {
            if stop_requested(&stop_rx) {
                break;
            }

            if self.fail_counter > HLS_MAX_RETRIES {
                if !send_queue_item(&queue_tx, SegmentQueue::StreamOver, &mut stop_rx).await? {
                    break;
                }
                break;
            }

            let (media_url, media_text) = match self.fetch_current_playlist_text(&mut stop_rx).await
            {
                Ok(Some(value)) => value,
                Ok(None) => break,
                Err(error) => {
                    self.fail_counter += 1;
                    if self.fail_counter > HLS_MAX_RETRIES {
                        if !send_queue_item(&queue_tx, SegmentQueue::StreamOver, &mut stop_rx)
                            .await?
                        {
                            return Ok(());
                        }
                        return Err(error);
                    }
                    if sleep_or_stop(self.timeout, &mut stop_rx).await {
                        break;
                    }
                    continue;
                }
            };
            let media_playlist = match parse_media_playlist(&media_text) {
                Ok(media_playlist) => media_playlist,
                Err(error) => {
                    self.fail_counter += 1;
                    if self.fail_counter > HLS_MAX_RETRIES {
                        if !send_queue_item(&queue_tx, SegmentQueue::StreamOver, &mut stop_rx)
                            .await?
                        {
                            return Ok(());
                        }
                        return Err(error);
                    }
                    if sleep_or_stop(self.timeout, &mut stop_rx).await {
                        break;
                    }
                    continue;
                }
            };

            let mut saw_new_segment = false;
            for (_, segment) in media_playlist.segments.iter() {
                if let Some(initialization_map) = initialization_map_request(
                    &self.provider_id,
                    &self.playlist_url,
                    &media_url,
                    &self.headers,
                    segment.map.as_ref(),
                    &mut self.active_init_map,
                )? {
                    if !send_queue_item(
                        &queue_tx,
                        SegmentQueue::Url(initialization_map),
                        &mut stop_rx,
                    )
                    .await?
                    {
                        return Ok(());
                    }
                    saw_new_segment = true;
                }

                let resolved = resolve_child_url(&media_url, segment.uri().trim())
                    .map_err(|error| error.to_string())?;
                validate_provider_url(&self.provider_id, resolved.as_str())
                    .map_err(|error| error.to_string())?;
                if self.links.insert(resolved.as_str()) {
                    let headers = filtered_headers(
                        self.playlist_url.as_str(),
                        resolved.as_str(),
                        &self.headers,
                    );
                    if !send_queue_item(
                        &queue_tx,
                        SegmentQueue::Url(SegmentRequest {
                            url: resolved,
                            headers,
                        }),
                        &mut stop_rx,
                    )
                    .await?
                    {
                        return Ok(());
                    }
                    saw_new_segment = true;
                }
            }

            if media_playlist.has_end_list {
                let _ = send_queue_item(&queue_tx, SegmentQueue::StreamOver, &mut stop_rx).await?;
                break;
            }

            self.fail_counter = if saw_new_segment {
                0
            } else {
                self.fail_counter + 1
            };
            if sleep_or_stop(
                bounded_playlist_reload(media_playlist.target_duration),
                &mut stop_rx,
            )
            .await
            {
                break;
            }
        }

        Ok(())
    }

    async fn fetch_current_playlist_text(
        &self,
        stop_rx: &mut watch::Receiver<bool>,
    ) -> Result<Option<(Url, String)>, String> {
        let Some(root_text) = self.fetch_text(self.playlist_url.as_ref(), stop_rx).await? else {
            return Ok(None);
        };
        if parse_media_playlist(&root_text).is_ok() {
            return Ok(Some((self.playlist_url.clone(), root_text)));
        }

        let media_error = parse_media_playlist(&root_text)
            .err()
            .unwrap_or_else(|| "invalid HLS playlist".to_owned());
        let master_playlist = MasterPlaylist::try_from(root_text.as_str())
            .map_err(|master_error| format!("{media_error}; {master_error}"))?;
        let variant_uri = select_master_variant(&master_playlist)
            .ok_or_else(|| "missing HLS variant stream".to_owned())?;
        let media_url = resolve_child_url(&self.playlist_url, variant_uri)
            .map_err(|error| error.to_string())?;
        validate_provider_url(&self.provider_id, media_url.as_str())
            .map_err(|error| error.to_string())?;
        let Some(media_text) = self.fetch_text(media_url.as_ref(), stop_rx).await? else {
            return Ok(None);
        };
        Ok(Some((media_url, media_text)))
    }

    async fn fetch_text(
        &self,
        target_url: &str,
        stop_rx: &mut watch::Receiver<bool>,
    ) -> Result<Option<String>, String> {
        validate_provider_url(&self.provider_id, target_url).map_err(|error| error.to_string())?;
        if stop_requested(stop_rx) {
            return Ok(None);
        }

        let response = tokio::select! {
            response = self.client
            .get(target_url)
            .headers(filtered_headers(
                self.playlist_url.as_str(),
                target_url,
                &self.headers,
            ))
            .timeout(self.timeout)
            .send() => response.map_err(|error| error.to_string())?,
            () = wait_for_stop(stop_rx) => return Ok(None),
        }
        .error_for_status()
        .map_err(|error| error.to_string())?;

        let text = tokio::time::timeout(self.timeout, read_playlist_body(response, stop_rx))
            .await
            .map_err(|_| "HLS playlist body timed out".to_owned())??;

        Ok(Some(text))
    }
}

async fn read_playlist_body(
    response: reqwest::Response,
    stop_rx: &mut watch::Receiver<bool>,
) -> Result<String, String> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_PLAYLIST_BODY_BYTES as u64)
    {
        return Err("HLS playlist body exceeded its size limit".to_owned());
    }
    let mut bytes = Vec::with_capacity(
        response
            .content_length()
            .unwrap_or_default()
            .min(MAX_PLAYLIST_BODY_BYTES as u64) as usize,
    );
    let mut stream = response.bytes_stream();
    while let Some(chunk) = tokio::select! {
        chunk = stream.next() => chunk,
        () = wait_for_stop(stop_rx) => return Err("HLS playlist fetch stopped".to_owned()),
    } {
        let chunk = chunk.map_err(|error| error.to_string())?;
        if bytes.len().saturating_add(chunk.len()) > MAX_PLAYLIST_BODY_BYTES {
            return Err("HLS playlist body exceeded its size limit".to_owned());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn parse_media_playlist(raw: &str) -> Result<MediaPlaylist<'_>, String> {
    let mut builder = MediaPlaylist::builder();
    builder.allowable_excess_duration(Duration::from_secs(10));
    builder.parse(raw).map_err(|error| error.to_string())
}

fn initialization_map_request(
    provider_id: &str,
    playlist_url: &Url,
    media_url: &Url,
    headers: &HeaderMap,
    map: Option<&ExtXMap<'_>>,
    active_map: &mut Option<String>,
) -> Result<Option<SegmentRequest>, String> {
    // EXT-X-MAP applies to following segments, but the parsed playlist may
    // expose it only on the segment where the tag appeared. Absence here must
    // therefore preserve the active map until a later map tag replaces it.
    let Some(map) = map else {
        return Ok(None);
    };

    if map.range().is_some() {
        return Err("HLS initialization map byte ranges are unsupported".to_owned());
    }
    if !map.keys().is_empty() {
        return Err("encrypted HLS initialization maps are unsupported".to_owned());
    }

    let resolved =
        resolve_child_url(media_url, map.uri().trim()).map_err(|error| error.to_string())?;
    validate_provider_url(provider_id, resolved.as_str()).map_err(|error| error.to_string())?;
    if active_map.as_deref() == Some(resolved.as_str()) {
        return Ok(None);
    }

    *active_map = Some(resolved.as_str().to_owned());
    Ok(Some(SegmentRequest {
        headers: filtered_headers(playlist_url.as_str(), resolved.as_str(), headers),
        url: resolved,
    }))
}

async fn send_queue_item(
    queue_tx: &Sender<SegmentQueue>,
    item: SegmentQueue,
    stop_rx: &mut watch::Receiver<bool>,
) -> Result<bool, String> {
    loop {
        if stop_requested(stop_rx) {
            return Ok(false);
        }

        tokio::select! {
            result = queue_tx.reserve() => {
                let permit = result.map_err(|error| error.to_string())?;
                permit.send(item);
                return Ok(true);
            }
            changed = stop_rx.changed() => {
                if changed.is_err() || stop_requested(stop_rx) {
                    return Ok(false);
                }
            }
        }
    }
}

async fn sleep_or_stop(duration: Duration, stop_rx: &mut watch::Receiver<bool>) -> bool {
    if stop_requested(stop_rx) {
        return true;
    }

    tokio::select! {
        () = tokio::time::sleep(duration) => stop_requested(stop_rx),
        changed = stop_rx.changed() => changed.is_err() || stop_requested(stop_rx),
    }
}

async fn wait_for_stop(stop_rx: &mut watch::Receiver<bool>) {
    loop {
        if stop_requested(stop_rx) || stop_rx.changed().await.is_err() {
            return;
        }
    }
}

fn stop_requested(stop_rx: &watch::Receiver<bool>) -> bool {
    *stop_rx.borrow()
}

fn request_stop(stop_tx: &watch::Sender<bool>) {
    let _ = stop_tx.send(true);
}

fn bounded_playlist_reload(duration: Duration) -> Duration {
    if duration < HLS_MIN_PLAYLIST_RELOAD {
        HLS_MIN_PLAYLIST_RELOAD
    } else if duration > HLS_MAX_PLAYLIST_RELOAD {
        HLS_MAX_PLAYLIST_RELOAD
    } else {
        duration
    }
}

struct RecentSegmentLinks {
    links: PatriciaSet,
    order: VecDeque<String>,
    capacity: usize,
}

impl RecentSegmentLinks {
    fn new(capacity: usize) -> Self {
        Self {
            links: PatriciaSet::new(),
            order: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    fn insert(&mut self, url: &str) -> bool {
        if !self.links.insert(url) {
            return false;
        }

        self.order.push_back(url.to_owned());
        while self.order.len() > self.capacity {
            if let Some(expired) = self.order.pop_front() {
                self.links.remove(expired.as_str());
            }
        }

        true
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.links.len()
    }
}

fn select_master_variant<'a>(master: &'a MasterPlaylist<'a>) -> Option<&'a str> {
    master
        .variant_streams
        .iter()
        .filter_map(variant_candidate)
        .min_by_key(|(_, score)| *score)
        .map(|(uri, _)| uri)
}

fn variant_candidate<'a>(variant: &'a VariantStream<'a>) -> Option<(&'a str, (u8, u64))> {
    match variant {
        VariantStream::ExtXStreamInf {
            uri,
            audio,
            stream_data,
            ..
        } => {
            let bandwidth = stream_data
                .average_bandwidth()
                .unwrap_or_else(|| stream_data.bandwidth());
            let codec_text = stream_data
                .codecs()
                .map(|codecs| codecs.to_string().to_ascii_lowercase())
                .unwrap_or_default();
            let has_video = stream_data.resolution().is_some()
                || stream_data.video().is_some()
                || codec_text.contains("avc1")
                || codec_text.contains("hvc1")
                || codec_text.contains("hev1")
                || codec_text.contains("av01")
                || codec_text.contains("vp9");
            let has_audio = codec_text.contains("mp4a")
                || codec_text.contains("opus")
                || codec_text.contains("ac-3")
                || codec_text.contains("ec-3");
            let class = match (has_video, has_audio, audio.is_some()) {
                (false, true, _) => 0,
                (false, false, true) => 1,
                (false, false, false) => 2,
                (true, _, _) => 3,
            };

            Some((uri.as_ref(), (class, bandwidth)))
        }
        VariantStream::ExtXIFrame { .. } => None,
    }
}

fn resolve_child_url(base: &Url, raw: &str) -> Result<Url, ParseError> {
    match Url::parse(raw) {
        Ok(url) => Ok(url),
        Err(ParseError::RelativeUrlWithoutBase) => base.join(raw),
        Err(error) => Err(error),
    }
}

struct ValidatedHlsAsyncSource {
    stream: DuplexStream,
    stop_tx: watch::Sender<bool>,
}

impl Drop for ValidatedHlsAsyncSource {
    fn drop(&mut self) {
        request_stop(&self.stop_tx);
    }
}

impl AsyncRead for ValidatedHlsAsyncSource {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncSeek for ValidatedHlsAsyncSource {
    fn start_seek(
        self: std::pin::Pin<&mut Self>,
        _position: std::io::SeekFrom,
    ) -> std::io::Result<()> {
        Err(std::io::ErrorKind::Unsupported.into())
    }

    fn poll_complete(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<u64>> {
        unreachable!()
    }
}

#[async_trait]
impl AsyncMediaSource for ValidatedHlsAsyncSource {
    fn is_seekable(&self) -> bool {
        false
    }

    async fn byte_len(&self) -> Option<u64> {
        None
    }

    async fn try_resume(
        &mut self,
        _offset: u64,
    ) -> Result<Box<dyn AsyncMediaSource>, AudioStreamError> {
        Err(AudioStreamError::Unsupported)
    }
}

#[async_trait]
impl Compose for ValidatedHlsRequest {
    fn create(&mut self) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
        self.create_stream().map(|input| {
            let stream = AsyncAdapterStream::new(Box::new(input), STREAM_BUFFER_BYTES);
            AudioStream {
                input: Box::new(stream) as Box<dyn MediaSource>,
            }
        })
    }

    async fn create_async(
        &mut self,
    ) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
        self.create()
    }

    fn should_create_async(&self) -> bool {
        true
    }
}

impl From<ValidatedHlsRequest> for Input {
    fn from(value: ValidatedHlsRequest) -> Self {
        Input::Lazy(Box::new(value))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::{SocketAddr, TcpListener},
        time::Duration,
    };

    use super::{
        HLS_MAX_PLAYLIST_RELOAD, HLS_MIN_PLAYLIST_RELOAD, HlsWatcher, RecentSegmentLinks,
        SEGMENT_QUEUE_CAPACITY, STREAM_BUFFER_BYTES, SegmentDownloadFailure, SegmentQueue,
        SegmentRequest, ValidatedHlsAsyncSource, bounded_playlist_reload, bytes_forwarder,
        initialization_map_request, parse_media_playlist, request_stop, resolve_child_url,
        select_master_variant, send_queue_item, sleep_or_stop,
    };
    use hls_m3u8::MasterPlaylist;
    use reqwest::{Client, Url, header::HeaderMap};
    use tokio::{
        io::{AsyncReadExt, duplex},
        sync::{mpsc::channel, watch},
        time::timeout,
    };

    #[test]
    fn resolves_relative_child_urls() {
        let base = reqwest::Url::parse("https://example.com/path/audio/index.m3u8").unwrap();
        let resolved = resolve_child_url(&base, "segment-1.ts").unwrap();
        assert_eq!(
            resolved.as_str(),
            "https://example.com/path/audio/segment-1.ts"
        );
    }

    #[test]
    fn parses_media_playlists() {
        let playlist =
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\nsegment-1.ts\n#EXT-X-ENDLIST\n";
        let parsed = parse_media_playlist(playlist).unwrap();
        assert_eq!(parsed.segments.iter().count(), 1);
        assert!(parsed.has_end_list);
    }

    #[test]
    fn media_playlist_initialization_map_is_resolved_and_queued_once_per_map() {
        let playlist = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\nsegment-1.m4s\n#EXTINF:4.0,\nsegment-2.m4s\n#EXT-X-ENDLIST\n";
        let parsed = parse_media_playlist(playlist).unwrap();
        let base = Url::parse("https://cf-media.sndcdn.com/audio/index.m3u8").unwrap();
        let mut active_map = None;
        let first = parsed.segments.iter().next().unwrap().1;
        let first_map = initialization_map_request(
            "soundcloud",
            &base,
            &base,
            &HeaderMap::new(),
            first.map.as_ref(),
            &mut active_map,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            first_map.url.as_str(),
            "https://cf-media.sndcdn.com/audio/init.mp4"
        );

        let second = parsed.segments.iter().nth(1).unwrap().1;
        assert!(
            initialization_map_request(
                "soundcloud",
                &base,
                &base,
                &HeaderMap::new(),
                second.map.as_ref(),
                &mut active_map,
            )
            .unwrap()
            .is_none()
        );
        assert_eq!(
            active_map.as_deref(),
            Some("https://cf-media.sndcdn.com/audio/init.mp4")
        );
    }

    #[test]
    fn initialization_map_must_pass_provider_host_validation() {
        let playlist = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MAP:URI=\"https://untrusted.invalid/init.mp4\"\n#EXTINF:4.0,\nsegment-1.m4s\n#EXT-X-ENDLIST\n";
        let parsed = parse_media_playlist(playlist).unwrap();
        let base = Url::parse("https://cf-media.sndcdn.com/audio/index.m3u8").unwrap();
        let segment = parsed.segments.iter().next().unwrap().1;
        assert!(
            initialization_map_request(
                "soundcloud",
                &base,
                &base,
                &HeaderMap::new(),
                segment.map.as_ref(),
                &mut None,
            )
            .is_err()
        );
    }

    #[test]
    fn selects_first_master_variant_uri() {
        let master = MasterPlaylist::try_from(
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=160000\naudio/index.m3u8\n",
        )
        .unwrap();
        assert_eq!(select_master_variant(&master), Some("audio/index.m3u8"));
    }

    #[test]
    fn selects_audio_or_lowest_bandwidth_master_variant() {
        let master = MasterPlaylist::try_from(
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=4500000,CODECS=\"avc1.640029,mp4a.40.2\",RESOLUTION=1920x1080\nvideo/high.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=128000,CODECS=\"mp4a.40.2\"\naudio/low.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=256000,CODECS=\"mp4a.40.2\"\naudio/high.m3u8\n",
        )
        .unwrap();

        assert_eq!(select_master_variant(&master), Some("audio/low.m3u8"));
    }

    #[test]
    fn bounds_playlist_reload_delay() {
        assert_eq!(
            bounded_playlist_reload(Duration::from_millis(1)),
            HLS_MIN_PLAYLIST_RELOAD
        );
        assert_eq!(
            bounded_playlist_reload(Duration::from_secs(3)),
            Duration::from_secs(3)
        );
        assert_eq!(
            bounded_playlist_reload(Duration::from_secs(60)),
            HLS_MAX_PLAYLIST_RELOAD
        );
    }

    #[test]
    fn segment_retry_policy_is_limited_to_transient_failures() {
        assert!(
            SegmentDownloadFailure::HttpStatus(reqwest::StatusCode::TOO_MANY_REQUESTS).retryable()
        );
        assert!(SegmentDownloadFailure::HttpStatus(reqwest::StatusCode::BAD_GATEWAY).retryable());
        assert!(!SegmentDownloadFailure::HttpStatus(reqwest::StatusCode::FORBIDDEN).retryable());
        assert!(!SegmentDownloadFailure::HttpStatus(reqwest::StatusCode::NOT_FOUND).retryable());
        assert!(!SegmentDownloadFailure::TooLarge.retryable());
    }

    #[test]
    fn evicts_old_segment_links() {
        let mut links = RecentSegmentLinks::new(2);

        assert!(links.insert("https://cdn.example.test/audio/segment-1.ts"));
        assert!(links.insert("https://cdn.example.test/audio/segment-2.ts"));
        assert!(!links.insert("https://cdn.example.test/audio/segment-1.ts"));
        assert!(links.insert("https://cdn.example.test/audio/segment-3.ts"));
        assert_eq!(links.len(), 2);
        assert!(links.insert("https://cdn.example.test/audio/segment-1.ts"));
        assert_eq!(links.len(), 2);
    }

    #[tokio::test]
    async fn dropping_reader_after_partial_read_stops_forwarder() {
        let segment_url = spawn_streaming_response(STREAM_BUFFER_BYTES * 4);
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let (segment_tx, segment_rx) = channel(SEGMENT_QUEUE_CAPACITY);
        let (stop_tx, mut observed_stop_rx) = watch::channel(false);
        let forwarder_stop_rx = observed_stop_rx.clone();
        let (writer, mut reader) = duplex(STREAM_BUFFER_BYTES);
        let forwarder = tokio::spawn(bytes_forwarder(
            client,
            segment_rx,
            writer,
            stop_tx,
            forwarder_stop_rx,
        ));

        segment_tx
            .send(SegmentQueue::Url(SegmentRequest {
                url: Url::parse(&segment_url).unwrap(),
                headers: HeaderMap::new(),
            }))
            .await
            .unwrap();

        let mut first_bytes = [0_u8; 2048];
        timeout(Duration::from_secs(2), reader.read_exact(&mut first_bytes))
            .await
            .unwrap()
            .unwrap();
        drop(reader);

        timeout(Duration::from_secs(2), async {
            while !*observed_stop_rx.borrow() {
                observed_stop_rx.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        timeout(Duration::from_secs(2), forwarder)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn transient_segment_body_failure_retries_without_duplicating_audio() {
        let expected = (0..4096)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        let (segment_url, server) = spawn_truncated_then_complete_response(expected.clone());
        let client = Client::builder()
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        let (segment_tx, segment_rx) = channel(SEGMENT_QUEUE_CAPACITY);
        let (stop_tx, stop_rx) = watch::channel(false);
        let (writer, mut reader) = duplex(STREAM_BUFFER_BYTES);
        let forwarder = tokio::spawn(bytes_forwarder(
            client, segment_rx, writer, stop_tx, stop_rx,
        ));

        segment_tx
            .send(SegmentQueue::Url(SegmentRequest {
                url: Url::parse(&segment_url).unwrap(),
                headers: HeaderMap::new(),
            }))
            .await
            .unwrap();
        segment_tx.send(SegmentQueue::StreamOver).await.unwrap();
        drop(segment_tx);

        let mut actual = Vec::new();
        timeout(Duration::from_secs(3), reader.read_to_end(&mut actual))
            .await
            .unwrap()
            .unwrap();
        timeout(Duration::from_secs(3), forwarder)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(actual, expected);
        assert_eq!(server.join().unwrap(), 2);
    }

    #[tokio::test]
    async fn dropping_async_source_notifies_stop_waiters() {
        let (stop_tx, mut stop_rx) = watch::channel(false);
        let (_writer, reader) = duplex(STREAM_BUFFER_BYTES);
        let source = ValidatedHlsAsyncSource {
            stream: reader,
            stop_tx,
        };

        drop(source);

        timeout(Duration::from_secs(1), async {
            while !*stop_rx.borrow() {
                stop_rx.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn playlist_sleep_returns_when_stop_is_requested() {
        let (stop_tx, mut stop_rx) = watch::channel(false);
        let waiter =
            tokio::spawn(async move { sleep_or_stop(Duration::from_secs(60), &mut stop_rx).await });

        tokio::time::sleep(Duration::from_millis(10)).await;
        request_stop(&stop_tx);

        assert!(
            timeout(Duration::from_secs(1), waiter)
                .await
                .unwrap()
                .unwrap()
        );
    }

    #[tokio::test]
    async fn queued_segment_send_returns_when_stopped_while_full() {
        let (queue_tx, mut queue_rx) = channel(1);
        queue_tx.send(SegmentQueue::StreamOver).await.unwrap();
        let (stop_tx, mut stop_rx) = watch::channel(false);
        let sender = tokio::spawn(async move {
            send_queue_item(&queue_tx, SegmentQueue::StreamOver, &mut stop_rx).await
        });

        tokio::time::sleep(Duration::from_millis(10)).await;
        request_stop(&stop_tx);

        assert!(
            !timeout(Duration::from_secs(1), sender)
                .await
                .unwrap()
                .unwrap()
                .unwrap()
        );
        assert!(queue_rx.recv().await.is_some());
    }

    #[tokio::test]
    async fn fetch_text_returns_when_stopped_during_http_request() {
        let local_addr = spawn_tls_blackhole();
        let client = Client::builder()
            .resolve("manifest.googlevideo.com", local_addr)
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap();
        let watcher = HlsWatcher::new(
            client,
            "youtube".to_owned(),
            "https://manifest.googlevideo.com/root.m3u8".to_owned(),
            HeaderMap::new(),
        )
        .unwrap();
        let (stop_tx, mut stop_rx) = watch::channel(false);
        let fetch = tokio::spawn(async move {
            watcher
                .fetch_text("https://manifest.googlevideo.com/root.m3u8", &mut stop_rx)
                .await
        });

        tokio::time::sleep(Duration::from_millis(10)).await;
        request_stop(&stop_tx);

        assert!(
            timeout(Duration::from_secs(1), fetch)
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .is_none()
        );
    }

    fn spawn_streaming_response(body_bytes: usize) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/segment.ts", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
            stream.set_write_timeout(Some(Duration::from_secs(2))).ok();

            let mut request = Vec::new();
            let mut read_buf = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                match stream.read(&mut read_buf) {
                    Ok(0) => return,
                    Ok(read) => request.extend_from_slice(&read_buf[..read]),
                    Err(_) => return,
                }
            }

            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {body_bytes}\r\nContent-Type: video/mp2t\r\nConnection: close\r\n\r\n"
            );
            if stream.write_all(header.as_bytes()).is_err() {
                return;
            }

            let chunk = [0x55_u8; 4096];
            let mut remaining = body_bytes;
            while remaining > 0 {
                let write_len = remaining.min(chunk.len());
                if stream.write_all(&chunk[..write_len]).is_err() {
                    return;
                }
                remaining -= write_len;
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        url
    }

    fn spawn_truncated_then_complete_response(
        body: Vec<u8>,
    ) -> (String, std::thread::JoinHandle<usize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/segment.ts", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let mut accepted = 0;
            for attempt in 0..2 {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                accepted += 1;
                stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
                stream.set_write_timeout(Some(Duration::from_secs(2))).ok();

                let mut request = Vec::new();
                let mut read_buf = [0_u8; 1024];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    match stream.read(&mut read_buf) {
                        Ok(0) => break,
                        Ok(read) => request.extend_from_slice(&read_buf[..read]),
                        Err(_) => break,
                    }
                }

                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: audio/mp4\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                if stream.write_all(header.as_bytes()).is_err() {
                    continue;
                }
                if attempt == 0 {
                    let partial_len = body.len() / 2;
                    let _ = stream.write_all(&body[..partial_len]);
                } else {
                    let _ = stream.write_all(&body);
                }
            }
            accepted
        });
        (url, server)
    }

    fn spawn_tls_blackhole() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
            let mut read_buf = [0_u8; 1024];
            let _ = stream.read(&mut read_buf);
            std::thread::sleep(Duration::from_secs(2));
        });
        addr
    }
}
