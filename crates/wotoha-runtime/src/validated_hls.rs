use std::{collections::VecDeque, time::Duration};

use async_trait::async_trait;
use futures::StreamExt;
use hls_m3u8::{MasterPlaylist, MediaPlaylist, tags::VariantStream};
use patricia_tree::PatriciaSet;
use reqwest::{Client, Request, Url, header::HeaderMap};
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
use url::ParseError;

use crate::hls_security::{filtered_headers, validate_provider_url};

const HLS_MAX_RETRIES: usize = 12;
const PLAYLIST_TIMEOUT: Duration = Duration::from_secs(10);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(10);
const HLS_MIN_PLAYLIST_RELOAD: Duration = Duration::from_millis(500);
const HLS_MAX_PLAYLIST_RELOAD: Duration = Duration::from_secs(15);
const SEGMENT_QUEUE_CAPACITY: usize = 16;
const SEGMENT_LINK_CAPACITY: usize = 512;
const STREAM_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub struct ValidatedHlsRequest {
    client: Client,
    provider_id: String,
    playlist_url: String,
    headers: HeaderMap,
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
        Self {
            client,
            provider_id: provider_id.into(),
            playlist_url,
            headers,
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
        tokio::spawn(async move {
            if let Err(error) = watcher.run(watcher_tx.clone(), watcher_stop_rx).await {
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
                let request = match client
                    .get(segment.url.clone())
                    .headers(segment.headers)
                    .timeout(DOWNLOAD_TIMEOUT)
                    .build()
                {
                    Ok(request) => request,
                    Err(error) => {
                        tracing::warn!("validated hls request build failed: {error}");
                        continue;
                    }
                };

                match download_segment(client.clone(), request, &mut writer, &mut stop_rx).await {
                    Ok(SegmentDownload::Completed) => {}
                    Ok(SegmentDownload::Stopped) => break,
                    Err(error) => {
                        tracing::warn!("validated hls segment download failed: {error}");
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

async fn download_segment(
    client: Client,
    request: Request,
    writer: &mut DuplexStream,
    stop_rx: &mut watch::Receiver<bool>,
) -> Result<SegmentDownload, String> {
    if stop_requested(stop_rx) {
        return Ok(SegmentDownload::Stopped);
    }

    let response = tokio::select! {
        response = client.execute(request) => response.map_err(|error| error.to_string())?,
        changed = stop_rx.changed() => {
            if changed.is_err() || stop_requested(stop_rx) {
                return Ok(SegmentDownload::Stopped);
            }
            return Err("HLS segment download stop signal changed unexpectedly".to_owned());
        }
    };

    let mut stream = response
        .error_for_status()
        .map_err(|error| error.to_string())?
        .bytes_stream();

    loop {
        if stop_requested(stop_rx) {
            return Ok(SegmentDownload::Stopped);
        }

        let next = tokio::select! {
            next = tokio::time::timeout(DOWNLOAD_TIMEOUT, stream.next()) => next,
            changed = stop_rx.changed() => {
                if changed.is_err() || stop_requested(stop_rx) {
                    return Ok(SegmentDownload::Stopped);
                }
                continue;
            }
        };

        match next {
            Ok(Some(Ok(bytes))) => {
                let write_result = tokio::select! {
                    result = writer.write_all(&bytes) => result,
                    changed = stop_rx.changed() => {
                        if changed.is_err() || stop_requested(stop_rx) {
                            return Ok(SegmentDownload::Stopped);
                        }
                        continue;
                    }
                };
                write_result.map_err(|error| error.to_string())?;
            }
            Ok(Some(Err(error))) => return Err(error.to_string()),
            Ok(None) => return Ok(SegmentDownload::Completed),
            Err(_) => return Err("HLS segment download timed out".to_owned()),
        }
    }
}

struct HlsWatcher {
    client: Client,
    provider_id: String,
    playlist_url: Url,
    headers: HeaderMap,
    links: RecentSegmentLinks,
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

        let text = tokio::select! {
            text = response.text() => text.map_err(|error| error.to_string())?,
            () = wait_for_stop(stop_rx) => return Ok(None),
        };

        Ok(Some(text))
    }
}

fn parse_media_playlist(raw: &str) -> Result<MediaPlaylist<'_>, String> {
    let mut builder = MediaPlaylist::builder();
    builder.allowable_excess_duration(Duration::from_secs(10));
    builder.parse(raw).map_err(|error| error.to_string())
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
        SEGMENT_QUEUE_CAPACITY, STREAM_BUFFER_BYTES, SegmentQueue, SegmentRequest,
        ValidatedHlsAsyncSource, bounded_playlist_reload, bytes_forwarder, parse_media_playlist,
        request_stop, resolve_child_url, select_master_variant, send_queue_item, sleep_or_stop,
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
