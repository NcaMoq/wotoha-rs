use std::{
    collections::HashMap,
    io::{Error as IoError, ErrorKind as IoErrorKind, Result as IoResult, SeekFrom},
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};

use aes::Aes128;
use async_trait::async_trait;
use cipher::{Block, BlockDecrypt, KeyInit};
use futures::StreamExt;
use reqwest::{Client, header::HeaderMap};
use songbird::input::{
    AsyncAdapterStream, AsyncMediaSource, AudioStream, AudioStreamError, Compose, Input,
    core::io::MediaSource,
};
use tokio::io::{AsyncRead, AsyncSeek, AsyncWriteExt, DuplexStream, ReadBuf, duplex};
use tokio_util::sync::CancellationToken;

use crate::hls_security::{filtered_headers, validate_provider_url};

const NICONICO_HLS_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const NICONICO_HLS_MAX_KEY_BYTES: usize = 1024;
const NICONICO_HLS_MAX_MEDIA_PART_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct NiconicoHlsRequest {
    client: Client,
    playlist_url: String,
    headers: HeaderMap,
}

impl NiconicoHlsRequest {
    pub fn new(client: Client, playlist_url: String, headers: HeaderMap) -> Self {
        Self {
            client,
            playlist_url,
            headers,
        }
    }

    async fn fetch_text(&self, url: &str) -> Result<String, AudioStreamError> {
        self.fetch_response(url)
            .await?
            .text()
            .await
            .map_err(reqwest_error)
    }

    async fn fetch_bytes(&self, url: &str, max_bytes: usize) -> Result<Vec<u8>, AudioStreamError> {
        let response = self.fetch_response(url).await?;
        collect_response_bytes_limited(response, max_bytes).await
    }

    async fn fetch_response(&self, url: &str) -> Result<reqwest::Response, AudioStreamError> {
        validate_provider_url("niconico", url)?;
        self.client
            .get(url)
            .headers(filtered_headers(&self.playlist_url, url, &self.headers))
            .timeout(NICONICO_HLS_REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(reqwest_error)?
            .error_for_status()
            .map_err(reqwest_error)
    }

    async fn create_stream(&mut self) -> Result<NiconicoAsyncSource, AudioStreamError> {
        let playlist_text = self.fetch_text(&self.playlist_url).await?;
        let playlist = ParsedPlaylist::parse(&self.playlist_url, &playlist_text)?;
        let client = self.clone();
        let mut key_cache = HashMap::new();
        for encryption in playlist
            .map
            .iter()
            .filter_map(|part| part.encryption.as_ref())
            .chain(
                playlist
                    .segments
                    .iter()
                    .filter_map(|segment| segment.encryption.as_ref()),
            )
        {
            if !key_cache.contains_key(encryption.key_url.as_str()) {
                let key_bytes = client
                    .fetch_bytes(&encryption.key_url, NICONICO_HLS_MAX_KEY_BYTES)
                    .await?;
                let key = key_bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| AudioStreamError::Fail("invalid AES-128 key length".into()))?;
                key_cache.insert(encryption.key_url.clone(), key);
            }
        }
        Ok(spawn_source_forwarder(client, playlist, key_cache))
    }
}

fn spawn_source_forwarder(
    client: NiconicoHlsRequest,
    playlist: ParsedPlaylist,
    key_cache: HashMap<String, [u8; 16]>,
) -> NiconicoAsyncSource {
    let failure = StreamFailureState::default();
    let reader_failure = failure.clone();
    let cancellation = CancellationToken::new();
    let task_cancellation = cancellation.clone();
    let (mut writer, reader) = duplex(64 * 1024);
    tokio::spawn(async move {
        let result =
            forward_playlist(client, playlist, key_cache, &mut writer, &task_cancellation).await;
        finish_stream_forwarding(result, &mut writer, failure).await;
    });

    NiconicoAsyncSource {
        stream: reader,
        failure: reader_failure,
        cancellation,
    }
}

async fn forward_playlist(
    client: NiconicoHlsRequest,
    playlist: ParsedPlaylist,
    key_cache: HashMap<String, [u8; 16]>,
    writer: &mut DuplexStream,
    cancellation: &CancellationToken,
) -> Result<(), StreamForwardError> {
    if let Some(map) = &playlist.map {
        forward_media_part(&client, map, &key_cache, writer, cancellation).await?;
    }

    for segment in &playlist.segments {
        if cancellation.is_cancelled() {
            return Err(StreamForwardError::ReaderClosed);
        }
        forward_media_part(&client, segment, &key_cache, writer, cancellation).await?;
    }

    Ok(())
}

async fn forward_media_part(
    client: &NiconicoHlsRequest,
    part: &MediaPart,
    key_cache: &HashMap<String, [u8; 16]>,
    writer: &mut DuplexStream,
    cancellation: &CancellationToken,
) -> Result<(), StreamForwardError> {
    let response = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(StreamForwardError::Cancelled),
        result = client.fetch_response(&part.url) => result?,
    };
    match part.encryption.as_ref() {
        Some(encryption) => {
            forward_encrypted_response(response, encryption, key_cache, writer, cancellation).await
        }
        None => forward_plain_response(response, writer, cancellation).await,
    }
}

async fn forward_plain_response(
    response: reqwest::Response,
    writer: &mut DuplexStream,
    cancellation: &CancellationToken,
) -> Result<(), StreamForwardError> {
    validate_response_content_length(&response, NICONICO_HLS_MAX_MEDIA_PART_BYTES)?;
    let mut total_bytes = 0;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = next_response_chunk(&mut stream, cancellation).await? {
        add_limited_body_len(
            &mut total_bytes,
            chunk.len(),
            NICONICO_HLS_MAX_MEDIA_PART_BYTES,
        )?;
        write_all_cancellable(writer, &chunk, cancellation).await?;
    }

    Ok(())
}

async fn forward_encrypted_response(
    response: reqwest::Response,
    encryption: &EncryptionContext,
    key_cache: &HashMap<String, [u8; 16]>,
    writer: &mut DuplexStream,
    cancellation: &CancellationToken,
) -> Result<(), StreamForwardError> {
    validate_response_content_length(&response, NICONICO_HLS_MAX_MEDIA_PART_BYTES)?;
    let mut total_bytes = 0;
    let mut decryptor = StreamingAes128CbcDecryptor::new(encryption, key_cache)?;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = next_response_chunk(&mut stream, cancellation).await? {
        add_limited_body_len(
            &mut total_bytes,
            chunk.len(),
            NICONICO_HLS_MAX_MEDIA_PART_BYTES,
        )?;
        decryptor.push(&chunk, writer, cancellation).await?;
    }
    decryptor.finish(writer, cancellation).await
}

async fn next_response_chunk(
    stream: &mut (impl futures::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Unpin),
    cancellation: &CancellationToken,
) -> Result<Option<bytes::Bytes>, StreamForwardError> {
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(StreamForwardError::Cancelled),
        chunk = stream.next() => chunk.transpose().map_err(reqwest_error).map_err(StreamForwardError::Source),
    }
}

async fn write_all_cancellable(
    writer: &mut DuplexStream,
    bytes: &[u8],
    cancellation: &CancellationToken,
) -> Result<(), StreamForwardError> {
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(StreamForwardError::Cancelled),
        result = writer.write_all(bytes) => result.map_err(|_| StreamForwardError::ReaderClosed),
    }
}

async fn finish_stream_forwarding(
    result: Result<(), StreamForwardError>,
    writer: &mut DuplexStream,
    failure: StreamFailureState,
) {
    match result {
        Ok(()) | Err(StreamForwardError::Cancelled | StreamForwardError::ReaderClosed) => {}
        Err(StreamForwardError::Source(error)) => {
            tracing::warn!("niconico hls stream failed: {error}");
            failure.record_audio_error(error);
        }
    }

    let _ = writer.shutdown().await;
}

#[derive(Debug)]
enum StreamForwardError {
    Source(AudioStreamError),
    Cancelled,
    ReaderClosed,
}

impl From<AudioStreamError> for StreamForwardError {
    fn from(error: AudioStreamError) -> Self {
        Self::Source(error)
    }
}

async fn collect_response_bytes_limited(
    response: reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>, AudioStreamError> {
    validate_response_content_length(&response, max_bytes)?;

    let capacity = response
        .content_length()
        .map(|content_length| content_length.min(max_bytes as u64) as usize)
        .unwrap_or_default();
    let mut bytes = Vec::with_capacity(capacity);
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(reqwest_error)?;
        append_limited_body_chunk(&mut bytes, &chunk, max_bytes)?;
    }

    Ok(bytes)
}

fn validate_response_content_length(
    response: &reqwest::Response,
    max_bytes: usize,
) -> Result<(), AudioStreamError> {
    if response
        .content_length()
        .is_some_and(|content_length| content_length > max_bytes as u64)
    {
        return Err(response_too_large_error(max_bytes));
    }
    Ok(())
}

fn append_limited_body_chunk(
    bytes: &mut Vec<u8>,
    chunk: &[u8],
    max_bytes: usize,
) -> Result<(), AudioStreamError> {
    let mut new_len = bytes.len();
    add_limited_body_len(&mut new_len, chunk.len(), max_bytes)?;
    bytes.extend_from_slice(chunk);
    Ok(())
}

fn add_limited_body_len(
    current: &mut usize,
    chunk_len: usize,
    max_bytes: usize,
) -> Result<(), AudioStreamError> {
    let new_len = current
        .checked_add(chunk_len)
        .ok_or_else(|| response_too_large_error(max_bytes))?;
    if new_len > max_bytes {
        return Err(response_too_large_error(max_bytes));
    }
    *current = new_len;
    Ok(())
}

fn response_too_large_error(max_bytes: usize) -> AudioStreamError {
    AudioStreamError::Fail(format!("NicoNico HLS response exceeded byte limit: {max_bytes}").into())
}

struct StreamingAes128CbcDecryptor {
    cipher: Aes128,
    previous_cipher_block: [u8; 16],
    partial_cipher_block: Vec<u8>,
    pending_plaintext_block: Option<[u8; 16]>,
}

impl StreamingAes128CbcDecryptor {
    fn new(
        encryption: &EncryptionContext,
        key_cache: &HashMap<String, [u8; 16]>,
    ) -> Result<Self, AudioStreamError> {
        let key = key_cache
            .get(encryption.key_url.as_str())
            .ok_or_else(|| AudioStreamError::Fail("missing AES-128 key bytes".into()))?;
        let cipher =
            Aes128::new_from_slice(key).map_err(|error| AudioStreamError::Fail(Box::new(error)))?;
        Ok(Self {
            cipher,
            previous_cipher_block: encryption.iv,
            partial_cipher_block: Vec::with_capacity(16),
            pending_plaintext_block: None,
        })
    }

    async fn push(
        &mut self,
        chunk: &[u8],
        writer: &mut DuplexStream,
        cancellation: &CancellationToken,
    ) -> Result<(), StreamForwardError> {
        let mut remaining = chunk;
        let mut plaintext = Vec::with_capacity(chunk.len());
        if !self.partial_cipher_block.is_empty() {
            let needed = 16 - self.partial_cipher_block.len();
            let take_len = needed.min(remaining.len());
            self.partial_cipher_block
                .extend_from_slice(&remaining[..take_len]);
            remaining = &remaining[take_len..];
            if self.partial_cipher_block.len() == 16 {
                let block = array_from_block(&self.partial_cipher_block);
                self.process_cipher_block(&block, &mut plaintext);
                self.partial_cipher_block.clear();
            } else {
                return Ok(());
            }
        }

        let complete_len = remaining.len() / 16 * 16;
        for block in remaining[..complete_len].chunks_exact(16) {
            let block = array_from_block(block);
            self.process_cipher_block(&block, &mut plaintext);
        }
        self.partial_cipher_block
            .extend_from_slice(&remaining[complete_len..]);
        if !plaintext.is_empty() {
            write_all_cancellable(writer, &plaintext, cancellation).await?;
        }
        Ok(())
    }

    fn process_cipher_block(&mut self, ciphertext_block: &[u8; 16], plaintext: &mut Vec<u8>) {
        let mut block = Block::<Aes128>::clone_from_slice(ciphertext_block);
        self.cipher.decrypt_block(&mut block);
        let mut plaintext_block = [0_u8; 16];
        for index in 0..16 {
            plaintext_block[index] = block[index] ^ self.previous_cipher_block[index];
        }
        self.previous_cipher_block = *ciphertext_block;
        if let Some(previous_plaintext_block) =
            self.pending_plaintext_block.replace(plaintext_block)
        {
            plaintext.extend_from_slice(&previous_plaintext_block);
        }
    }

    async fn finish(
        mut self,
        writer: &mut DuplexStream,
        cancellation: &CancellationToken,
    ) -> Result<(), StreamForwardError> {
        if !self.partial_cipher_block.is_empty() {
            return Err(invalid_pkcs7_error("trailing partial AES block").into());
        }

        let Some(last_block) = self.pending_plaintext_block.take() else {
            return Err(invalid_pkcs7_error("missing AES block").into());
        };
        let plaintext_len = pkcs7_unpadded_len(&last_block)?;
        write_all_cancellable(writer, &last_block[..plaintext_len], cancellation).await
    }
}

fn array_from_block(block: &[u8]) -> [u8; 16] {
    block
        .try_into()
        .expect("AES block processing only passes 16 byte slices")
}

fn pkcs7_unpadded_len(block: &[u8; 16]) -> Result<usize, AudioStreamError> {
    let padding_len = block[15] as usize;
    if !(1..=16).contains(&padding_len) {
        return Err(invalid_pkcs7_error("invalid padding length"));
    }

    if block[16 - padding_len..]
        .iter()
        .any(|byte| *byte as usize != padding_len)
    {
        return Err(invalid_pkcs7_error("invalid padding bytes"));
    }

    Ok(16 - padding_len)
}

fn invalid_pkcs7_error(reason: &str) -> AudioStreamError {
    AudioStreamError::Fail(format!("invalid NicoNico AES-128 payload: {reason}").into())
}

#[derive(Clone, Debug, Default)]
struct StreamFailureState {
    failure: Arc<Mutex<Option<StreamFailure>>>,
}

impl StreamFailureState {
    fn record_audio_error(&self, error: AudioStreamError) {
        let kind = match &error {
            AudioStreamError::RetryIn(_) => IoErrorKind::TimedOut,
            AudioStreamError::Fail(_) => IoErrorKind::InvalidData,
            AudioStreamError::Unsupported => IoErrorKind::Unsupported,
            _ => IoErrorKind::Other,
        };
        self.record(kind, error.to_string());
    }

    fn record(&self, kind: IoErrorKind, message: impl Into<String>) {
        let mut failure = self
            .failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if failure.is_none() {
            *failure = Some(StreamFailure {
                kind,
                message: message.into(),
            });
        }
    }

    fn take_io_error(&self) -> Option<IoError> {
        self.failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
            .map(StreamFailure::into_io_error)
    }
}

#[derive(Clone, Debug)]
struct StreamFailure {
    kind: IoErrorKind,
    message: String,
}

impl StreamFailure {
    fn into_io_error(self) -> IoError {
        IoError::new(self.kind, self.message)
    }
}

fn reqwest_error(error: reqwest::Error) -> AudioStreamError {
    AudioStreamError::Fail(Box::new(error))
}

#[derive(Clone, Debug)]
struct ParsedPlaylist {
    map: Option<MediaPart>,
    segments: Vec<MediaPart>,
}

impl ParsedPlaylist {
    fn parse(base_url: &str, playlist: &str) -> Result<Self, AudioStreamError> {
        let base = reqwest::Url::parse(base_url)
            .map_err(|error| AudioStreamError::Fail(Box::new(error)))?;
        let mut current_encryption = None;
        let mut media_sequence = 0_u64;
        let mut segment_index = 0_u64;
        let mut map = None;
        let mut segments = Vec::new();

        for raw_line in playlist.lines() {
            let line = raw_line.trim();
            if line.is_empty() {
                continue;
            }

            if let Some(value) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
                media_sequence = value.parse().unwrap_or_default();
                continue;
            }

            if let Some(attrs) = line.strip_prefix("#EXT-X-MAP:") {
                let url = extract_attr(attrs, "URI")
                    .ok_or_else(|| AudioStreamError::Fail("missing EXT-X-MAP URI".into()))?;
                let resolved = base
                    .join(url)
                    .map_err(|error| AudioStreamError::Fail(Box::new(error)))?;
                map = Some(MediaPart {
                    url: resolved.to_string(),
                    encryption: current_encryption.clone(),
                });
                continue;
            }

            if let Some(attrs) = line.strip_prefix("#EXT-X-KEY:") {
                current_encryption = Some(parse_key(&base, attrs, media_sequence + segment_index)?);
                continue;
            }

            if line.starts_with('#') {
                continue;
            }

            let resolved = base
                .join(line)
                .map_err(|error| AudioStreamError::Fail(Box::new(error)))?;
            segments.push(MediaPart {
                url: resolved.to_string(),
                encryption: current_encryption.clone(),
            });
            segment_index += 1;
        }

        Ok(Self { map, segments })
    }
}

#[derive(Clone, Debug)]
struct MediaPart {
    url: String,
    encryption: Option<EncryptionContext>,
}

#[derive(Clone, Debug)]
struct EncryptionContext {
    key_url: String,
    iv: [u8; 16],
}

fn parse_key(
    base: &reqwest::Url,
    attrs: &str,
    media_sequence: u64,
) -> Result<EncryptionContext, AudioStreamError> {
    let method = extract_attr(attrs, "METHOD")
        .ok_or_else(|| AudioStreamError::Fail("missing EXT-X-KEY METHOD".into()))?;
    if method != "AES-128" {
        return Err(AudioStreamError::Fail(
            format!("unsupported NicoNico key method: {method}").into(),
        ));
    }

    let key_uri = extract_attr(attrs, "URI")
        .ok_or_else(|| AudioStreamError::Fail("missing EXT-X-KEY URI".into()))?;
    let key_url = base
        .join(key_uri)
        .map_err(|error| AudioStreamError::Fail(Box::new(error)))?;

    let iv = extract_attr(attrs, "IV")
        .map(parse_iv)
        .transpose()?
        .unwrap_or_else(|| iv_from_media_sequence(media_sequence));

    Ok(EncryptionContext {
        key_url: key_url.to_string(),
        iv,
    })
}

fn extract_attr<'a>(attrs: &'a str, name: &str) -> Option<&'a str> {
    let quoted = format!("{name}=\"");
    if let Some(start) = attrs.find(&quoted) {
        let tail = &attrs[start + quoted.len()..];
        let end = tail.find('"')?;
        return Some(&tail[..end]);
    }

    let bare = format!("{name}=");
    let start = attrs.find(&bare)?;
    let tail = &attrs[start + bare.len()..];
    let end = tail.find(',').unwrap_or(tail.len());
    Some(&tail[..end])
}

fn parse_iv(value: &str) -> Result<[u8; 16], AudioStreamError> {
    let hex = value.strip_prefix("0x").unwrap_or(value);
    if hex.len() != 32 {
        return Err(AudioStreamError::Fail("invalid AES IV length".into()));
    }

    let mut out = [0_u8; 16];
    for (index, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let chunk =
            std::str::from_utf8(chunk).map_err(|error| AudioStreamError::Fail(Box::new(error)))?;
        out[index] = u8::from_str_radix(chunk, 16)
            .map_err(|error| AudioStreamError::Fail(Box::new(error)))?;
    }
    Ok(out)
}

fn iv_from_media_sequence(sequence: u64) -> [u8; 16] {
    let mut out = [0_u8; 16];
    out[8..].copy_from_slice(&sequence.to_be_bytes());
    out
}

#[async_trait]
impl Compose for NiconicoHlsRequest {
    fn create(&mut self) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
        Err(AudioStreamError::Unsupported)
    }

    async fn create_async(
        &mut self,
    ) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
        self.create_stream().await.map(|input| {
            let stream = AsyncAdapterStream::new(Box::new(input), 64 * 1024);
            AudioStream {
                input: Box::new(stream) as Box<dyn MediaSource>,
            }
        })
    }

    fn should_create_async(&self) -> bool {
        true
    }
}

struct NiconicoAsyncSource {
    stream: DuplexStream,
    failure: StreamFailureState,
    cancellation: CancellationToken,
}

impl Drop for NiconicoAsyncSource {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

impl AsyncRead for NiconicoAsyncSource {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<IoResult<()>> {
        let had_remaining = buf.remaining() > 0;
        let before = buf.filled().len();
        match Pin::new(&mut self.stream).poll_read(cx, buf) {
            Poll::Ready(Ok(())) if had_remaining && buf.filled().len() == before => {
                if let Some(error) = self.failure.take_io_error() {
                    Poll::Ready(Err(error))
                } else {
                    Poll::Ready(Ok(()))
                }
            }
            other => other,
        }
    }
}

impl AsyncSeek for NiconicoAsyncSource {
    fn start_seek(self: Pin<&mut Self>, _position: SeekFrom) -> IoResult<()> {
        Err(IoErrorKind::Unsupported.into())
    }

    fn poll_complete(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<IoResult<u64>> {
        unreachable!()
    }
}

#[async_trait]
impl AsyncMediaSource for NiconicoAsyncSource {
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

impl From<NiconicoHlsRequest> for Input {
    fn from(value: NiconicoHlsRequest) -> Self {
        Input::Lazy(Box::new(value))
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, io::ErrorKind};

    use cbc::Encryptor;
    use cipher::{BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};
    use reqwest::{Client, header::HeaderMap};
    use songbird::input::AudioStreamError;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};
    use tokio_util::sync::CancellationToken;

    use super::{
        EncryptionContext, MediaPart, NiconicoAsyncSource, NiconicoHlsRequest, ParsedPlaylist,
        StreamFailureState, StreamForwardError, StreamingAes128CbcDecryptor, add_limited_body_len,
        append_limited_body_chunk, extract_attr, finish_stream_forwarding, forward_media_part,
        iv_from_media_sequence, parse_iv, response_too_large_error, spawn_source_forwarder,
    };

    #[test]
    fn parses_hls_attributes() {
        assert_eq!(
            extract_attr("METHOD=AES-128,URI=\"https://example.com/key\"", "METHOD"),
            Some("AES-128")
        );
        assert_eq!(
            extract_attr("METHOD=AES-128,URI=\"https://example.com/key\"", "URI"),
            Some("https://example.com/key")
        );
    }

    #[test]
    fn parses_hex_iv() {
        assert_eq!(
            parse_iv("0x00000000000000000000000000000001").unwrap()[15],
            1
        );
    }

    #[test]
    fn builds_default_iv_from_media_sequence() {
        let iv = iv_from_media_sequence(7);
        assert_eq!(&iv[8..], &7_u64.to_be_bytes());
    }

    #[tokio::test]
    async fn rejects_invalid_aes_ciphertext() {
        let mut key_cache = HashMap::new();
        key_cache.insert("https://asset.domand.nicovideo.jp/key".to_owned(), [0; 16]);
        let encryption = EncryptionContext {
            key_url: "https://asset.domand.nicovideo.jp/key".to_owned(),
            iv: [0; 16],
        };
        let mut decryptor = StreamingAes128CbcDecryptor::new(&encryption, &key_cache).unwrap();
        let (mut writer, _reader) = duplex(64);
        let cancellation = CancellationToken::new();

        decryptor
            .push(&[0; 15], &mut writer, &cancellation)
            .await
            .unwrap();
        let error = decryptor
            .finish(&mut writer, &cancellation)
            .await
            .unwrap_err();

        assert_source_message(error, "trailing partial AES block");
    }

    #[tokio::test]
    async fn decrypts_encrypted_media_part_incrementally() {
        let key = [7; 16];
        let iv = [9; 16];
        let plaintext = b"incremental niconico hls ciphertext";
        let mut buffer = vec![0_u8; plaintext.len() + 16];
        buffer[..plaintext.len()].copy_from_slice(plaintext);
        let ciphertext = Encryptor::<aes::Aes128>::new_from_slices(&key, &iv)
            .unwrap()
            .encrypt_padded_mut::<Pkcs7>(&mut buffer, plaintext.len())
            .unwrap()
            .to_vec();
        let key_url = "https://asset.domand.nicovideo.jp/key".to_owned();
        let mut key_cache = HashMap::new();
        key_cache.insert(key_url.clone(), key);
        let encryption = EncryptionContext { key_url, iv };
        let mut decryptor = StreamingAes128CbcDecryptor::new(&encryption, &key_cache).unwrap();
        let (mut writer, mut reader) = duplex(128);
        let cancellation = CancellationToken::new();

        decryptor
            .push(&ciphertext[..5], &mut writer, &cancellation)
            .await
            .unwrap();
        decryptor
            .push(&ciphertext[5..23], &mut writer, &cancellation)
            .await
            .unwrap();
        decryptor
            .push(&ciphertext[23..], &mut writer, &cancellation)
            .await
            .unwrap();
        decryptor.finish(&mut writer, &cancellation).await.unwrap();
        drop(writer);
        let mut out = Vec::new();
        reader.read_to_end(&mut out).await.unwrap();

        assert_eq!(out, plaintext);
    }

    #[test]
    fn rejects_body_chunks_over_limit() {
        let mut bytes = vec![1, 2, 3];
        let error = append_limited_body_chunk(&mut bytes, &[4, 5], 4).unwrap_err();

        assert!(error.to_string().contains("exceeded byte limit"));
        assert_eq!(bytes, vec![1, 2, 3]);
    }

    #[test]
    fn tracks_body_limit_without_extending_buffer() {
        let mut total_bytes = 3;
        let error = add_limited_body_len(&mut total_bytes, 2, 4).unwrap_err();

        assert!(error.to_string().contains("exceeded byte limit"));
        assert_eq!(total_bytes, 3);
    }

    #[test]
    fn dropping_source_cancels_forwarder() {
        let cancellation = CancellationToken::new();
        let (_, reader) = duplex(64);
        let source = NiconicoAsyncSource {
            stream: reader,
            failure: StreamFailureState::default(),
            cancellation: cancellation.clone(),
        };

        drop(source);

        assert!(cancellation.is_cancelled());
    }

    #[tokio::test]
    async fn completes_empty_playlist_without_reader_error() {
        let request = NiconicoHlsRequest::new(
            Client::new(),
            "https://www.nicovideo.jp/watch/sm9".to_owned(),
            HeaderMap::new(),
        );
        let playlist = ParsedPlaylist {
            map: None,
            segments: Vec::new(),
        };
        let mut source = spawn_source_forwarder(request, playlist, HashMap::new());
        let mut bytes = Vec::new();

        source.read_to_end(&mut bytes).await.unwrap();

        assert!(bytes.is_empty());
    }

    #[tokio::test]
    async fn reader_closed_does_not_record_stream_failure() {
        let failure = StreamFailureState::default();
        let reader_failure = failure.clone();
        let (mut writer, reader) = duplex(64);

        drop(reader);
        finish_stream_forwarding(Err(StreamForwardError::ReaderClosed), &mut writer, failure).await;

        assert!(reader_failure.take_io_error().is_none());
    }

    #[tokio::test]
    async fn cancelled_fetch_is_normal_stop() {
        let request = NiconicoHlsRequest::new(
            Client::new(),
            "https://www.nicovideo.jp/watch/sm9".to_owned(),
            HeaderMap::new(),
        );
        let part = MediaPart {
            url: "https://example.com/segment.ts".to_owned(),
            encryption: None,
        };
        let key_cache = HashMap::new();
        let (mut writer, _reader) = duplex(64);
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        let result =
            forward_media_part(&request, &part, &key_cache, &mut writer, &cancellation).await;

        assert!(matches!(result, Err(StreamForwardError::Cancelled)));
    }

    #[tokio::test]
    async fn reports_segment_fetch_failure_to_async_reader() {
        let request = NiconicoHlsRequest::new(
            Client::new(),
            "https://www.nicovideo.jp/watch/sm9".to_owned(),
            HeaderMap::new(),
        );
        let playlist = ParsedPlaylist {
            map: None,
            segments: vec![MediaPart {
                url: "https://example.com/segment.ts".to_owned(),
                encryption: None,
            }],
        };
        let mut source = spawn_source_forwarder(request, playlist, HashMap::new());
        let mut bytes = Vec::new();

        let error = source.read_to_end(&mut bytes).await.unwrap_err();

        assert_eq!(error.kind(), ErrorKind::InvalidData);
        assert!(error.to_string().contains("unsafe HLS child target"));
    }

    #[tokio::test]
    async fn reports_forwarding_failure_after_buffered_bytes() {
        let failure = StreamFailureState::default();
        let reader_failure = failure.clone();
        let (mut writer, reader) = duplex(64);
        let mut source = NiconicoAsyncSource {
            stream: reader,
            failure: reader_failure,
            cancellation: CancellationToken::new(),
        };
        tokio::spawn(async move {
            writer.write_all(b"abc").await.unwrap();
            finish_stream_forwarding(
                Err(StreamForwardError::Source(AudioStreamError::Fail(
                    "decrypt failed".into(),
                ))),
                &mut writer,
                failure,
            )
            .await;
        });
        let mut bytes = Vec::new();

        let error = source.read_to_end(&mut bytes).await.unwrap_err();

        assert_eq!(bytes, b"abc");
        assert_eq!(error.kind(), ErrorKind::InvalidData);
        assert!(error.to_string().contains("decrypt failed"));
    }

    #[tokio::test]
    async fn reports_size_limit_failure_to_async_reader() {
        let failure = StreamFailureState::default();
        let reader_failure = failure.clone();
        let (mut writer, reader) = duplex(64);
        let mut source = NiconicoAsyncSource {
            stream: reader,
            failure: reader_failure,
            cancellation: CancellationToken::new(),
        };
        tokio::spawn(async move {
            finish_stream_forwarding(
                Err(StreamForwardError::Source(response_too_large_error(4))),
                &mut writer,
                failure,
            )
            .await;
        });
        let mut bytes = Vec::new();

        let error = source.read_to_end(&mut bytes).await.unwrap_err();

        assert!(bytes.is_empty());
        assert_eq!(error.kind(), ErrorKind::InvalidData);
        assert!(error.to_string().contains("exceeded byte limit"));
    }

    fn assert_source_message(error: StreamForwardError, expected: &str) {
        let StreamForwardError::Source(error) = error else {
            panic!("unexpected stream result: {error:?}");
        };
        assert!(error.to_string().contains(expected));
    }
}
