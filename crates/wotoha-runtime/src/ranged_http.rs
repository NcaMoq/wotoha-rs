use std::{
    io::{Error as IoError, ErrorKind as IoErrorKind, Result as IoResult, SeekFrom},
    pin::Pin,
    task::{Context, Poll},
};

use async_trait::async_trait;
use futures::TryStreamExt;
use pin_project::pin_project;
use reqwest::{
    Client, StatusCode, Url,
    header::{ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, HeaderMap, RANGE},
};
use songbird::input::{
    AsyncAdapterStream, AsyncMediaSource, AudioStream, AudioStreamError, Compose, Input,
    core::io::MediaSource,
};
use tokio::io::{AsyncRead, AsyncSeek, ReadBuf};
use tokio_util::io::StreamReader;
use wotoha_core::{PreparedRangeMode, debug::append_debug_log};

#[derive(Clone, Debug)]
pub struct RangedHttpRequest {
    pub client: Client,
    pub request: String,
    pub headers: HeaderMap,
    pub content_length: Option<u64>,
    pub range_chunk_size: u64,
    pub range_mode: PreparedRangeMode,
}

impl RangedHttpRequest {
    #[must_use]
    pub fn new_with_headers(
        client: Client,
        request: String,
        headers: HeaderMap,
        content_length: Option<u64>,
        range_chunk_size: u64,
        range_mode: PreparedRangeMode,
    ) -> Self {
        Self {
            client,
            request,
            headers,
            content_length,
            range_chunk_size,
            range_mode,
        }
    }

    fn bounded_range_header(&self, offset: u64) -> String {
        let end = offset
            .saturating_add(self.range_chunk_size)
            .saturating_sub(1);
        match self.content_length {
            Some(content_length) => {
                format!(
                    "bytes={offset}-{}",
                    end.min(content_length.saturating_sub(1))
                )
            }
            None => format!("bytes={offset}-{end}"),
        }
    }

    fn range_candidates(&self, offset: u64) -> Vec<String> {
        if self.content_length.is_none() {
            return vec![format!("bytes={offset}-")];
        }

        if self.range_mode == PreparedRangeMode::QueryParam || offset == 0 {
            vec![self.bounded_range_header(offset)]
        } else {
            vec![
                format!("bytes={offset}-"),
                self.bounded_range_header(offset),
            ]
        }
    }

    fn request_url(&self, range_header: &str) -> Result<String, AudioStreamError> {
        if self.range_mode != PreparedRangeMode::QueryParam {
            return Ok(self.request.clone());
        }

        let mut url =
            Url::parse(&self.request).map_err(|error| AudioStreamError::Fail(Box::new(error)))?;
        let range_value = range_header.strip_prefix("bytes=").unwrap_or(range_header);
        url.query_pairs_mut().append_pair("range", range_value);
        Ok(url.to_string())
    }

    async fn create_stream(&mut self, offset: u64) -> Result<RangedHttpStream, AudioStreamError> {
        let mut last_status = None;
        for range_header in self.range_candidates(offset) {
            let request_url = self.request_url(&range_header)?;
            append_debug_log(format!(
                "ranged_http: request offset={} range={} url={}",
                offset, range_header, request_url
            ));
            let mut request = self.client.get(&request_url).headers(self.headers.clone());
            if self.range_mode != PreparedRangeMode::QueryParam {
                request = request.header(RANGE, &range_header);
            }
            let response = request
                .send()
                .await
                .map_err(|error| AudioStreamError::Fail(Box::new(error)))?;
            append_debug_log(format!(
                "ranged_http: response offset={} range={} status={}",
                offset,
                range_header,
                response.status()
            ));

            if !response.status().is_success() {
                last_status = Some(response.status());
                continue;
            }

            let expected_body_length = range_body_length(&range_header);
            if !response_matches_requested_offset(
                self.range_mode,
                response.status(),
                response.headers(),
                offset,
                expected_body_length,
            ) {
                append_debug_log(format!(
                    "ranged_http: rejected response offset={} range={} status={}",
                    offset,
                    range_header,
                    response.status()
                ));
                last_status = Some(response.status());
                continue;
            }

            let total_length = self
                .content_length
                .or_else(|| parse_content_range_total(response.headers()));
            let resume_supported = (self.range_mode == PreparedRangeMode::QueryParam
                && total_length.is_some())
                || response.status() == StatusCode::PARTIAL_CONTENT
                || response.headers().contains_key(CONTENT_RANGE)
                || response
                    .headers()
                    .get(ACCEPT_RANGES)
                    .and_then(|value| value.to_str().ok())
                    .is_some_and(|value| value == "bytes");
            let resume = (resume_supported && total_length.is_some()).then(|| self.clone());
            let stream = Box::new(StreamReader::new(
                response.bytes_stream().map_err(IoError::other),
            ));

            return Ok(RangedHttpStream {
                stream,
                total_length,
                resume,
                start_offset: offset,
                bytes_read: 0,
            });
        }
        let message: Box<dyn std::error::Error + Send + Sync + 'static> = format!(
            "failed with http status code: {}",
            last_status
                .map(|status| status.to_string())
                .unwrap_or_else(|| "unknown".to_owned())
        )
        .into();
        Err(AudioStreamError::Fail(message))
    }
}

fn parse_content_range_total(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(CONTENT_RANGE)?
        .to_str()
        .ok()?
        .split('/')
        .nth(1)?
        .parse()
        .ok()
}

fn parse_content_range_start(headers: &HeaderMap) -> Option<u64> {
    let value = headers.get(CONTENT_RANGE)?.to_str().ok()?;
    let range = value.strip_prefix("bytes ")?;
    range.split(['-', '/']).next()?.parse().ok()
}

fn parse_content_length(headers: &HeaderMap) -> Option<u64> {
    headers.get(CONTENT_LENGTH)?.to_str().ok()?.parse().ok()
}

fn range_body_length(range_header: &str) -> Option<u64> {
    let range = range_header.strip_prefix("bytes=").unwrap_or(range_header);
    let (start, end) = range.split_once('-')?;
    if end.is_empty() {
        return None;
    }

    let start = start.parse::<u64>().ok()?;
    let end = end.parse::<u64>().ok()?;
    end.checked_sub(start)?.checked_add(1)
}

fn response_matches_requested_offset(
    range_mode: PreparedRangeMode,
    status: StatusCode,
    headers: &HeaderMap,
    offset: u64,
    expected_body_length: Option<u64>,
) -> bool {
    if status == StatusCode::PARTIAL_CONTENT {
        return parse_content_range_start(headers) == Some(offset);
    }

    if range_mode == PreparedRangeMode::QueryParam && status == StatusCode::OK {
        if offset == 0 {
            return true;
        }

        return match (expected_body_length, parse_content_length(headers)) {
            (Some(expected), Some(actual)) => actual > 0 && actual <= expected,
            _ => true,
        };
    }

    offset == 0
}

#[pin_project]
struct RangedHttpStream {
    #[pin]
    stream: Box<dyn AsyncRead + Send + Sync + Unpin>,
    total_length: Option<u64>,
    resume: Option<RangedHttpRequest>,
    start_offset: u64,
    bytes_read: u64,
}

impl AsyncRead for RangedHttpStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<IoResult<()>> {
        let mut this = self.project();
        let before = buf.filled().len();
        match AsyncRead::poll_read(this.stream.as_mut(), cx, buf) {
            Poll::Ready(Ok(())) => {
                let after = buf.filled().len();
                let read = after.saturating_sub(before) as u64;
                if read > 0 {
                    *this.bytes_read += read;
                    return Poll::Ready(Ok(()));
                }

                let consumed = this.start_offset.saturating_add(*this.bytes_read);
                let should_resume = this
                    .total_length
                    .is_some_and(|total_length| consumed < total_length)
                    && this.resume.is_some();
                if should_resume && this.resume.is_some() {
                    append_debug_log(format!(
                        "ranged_http: chunk eof at offset={} consumed={} requesting resume",
                        this.start_offset, consumed
                    ));
                    return Poll::Ready(Err(IoError::new(
                        IoErrorKind::UnexpectedEof,
                        "range chunk exhausted before full stream was read",
                    )));
                }

                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

impl AsyncSeek for RangedHttpStream {
    fn start_seek(self: Pin<&mut Self>, _position: SeekFrom) -> IoResult<()> {
        Err(IoErrorKind::Unsupported.into())
    }

    fn poll_complete(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<IoResult<u64>> {
        unreachable!()
    }
}

#[async_trait]
impl AsyncMediaSource for RangedHttpStream {
    fn is_seekable(&self) -> bool {
        false
    }

    async fn byte_len(&self) -> Option<u64> {
        self.total_length
    }

    async fn try_resume(
        &mut self,
        offset: u64,
    ) -> Result<Box<dyn AsyncMediaSource>, AudioStreamError> {
        if let Some(resume) = &mut self.resume {
            resume
                .create_stream(offset)
                .await
                .map(|stream| Box::new(stream) as Box<dyn AsyncMediaSource>)
        } else {
            Err(AudioStreamError::Unsupported)
        }
    }
}

#[async_trait]
impl Compose for RangedHttpRequest {
    fn create(&mut self) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
        Err(AudioStreamError::Unsupported)
    }

    async fn create_async(
        &mut self,
    ) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
        self.create_stream(0).await.map(|input| {
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

impl From<RangedHttpRequest> for Input {
    fn from(value: RangedHttpRequest) -> Self {
        Input::Lazy(Box::new(value))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        io::ErrorKind,
        io::{Read, Write},
        net::TcpListener,
        time::Duration,
    };

    use super::{
        RangedHttpRequest, parse_content_range_start, range_body_length,
        response_matches_requested_offset,
    };
    use reqwest::{
        StatusCode,
        header::{CONTENT_LENGTH, CONTENT_RANGE, HeaderMap, HeaderValue},
    };
    use tokio::io::AsyncReadExt;
    use wotoha_core::PreparedRangeMode;

    #[test]
    fn validates_partial_content_start_offset() {
        let mut headers = HeaderMap::new();
        headers.insert(
            CONTENT_RANGE,
            HeaderValue::from_static("bytes 1024-2047/4096"),
        );

        assert_eq!(parse_content_range_start(&headers), Some(1024));
        assert!(response_matches_requested_offset(
            PreparedRangeMode::Header,
            StatusCode::PARTIAL_CONTENT,
            &headers,
            1024,
            range_body_length("bytes=1024-2047"),
        ));
        assert!(!response_matches_requested_offset(
            PreparedRangeMode::Header,
            StatusCode::PARTIAL_CONTENT,
            &headers,
            2048,
            range_body_length("bytes=2048-3071"),
        ));
        assert!(!response_matches_requested_offset(
            PreparedRangeMode::Header,
            StatusCode::OK,
            &headers,
            1024,
            range_body_length("bytes=1024-2047"),
        ));
        assert!(response_matches_requested_offset(
            PreparedRangeMode::Header,
            StatusCode::OK,
            &headers,
            0,
            range_body_length("bytes=0-1023"),
        ));
    }

    #[test]
    fn accepts_compatible_query_param_ok_response_at_resume_offset() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_LENGTH, HeaderValue::from_static("1024"));

        assert!(response_matches_requested_offset(
            PreparedRangeMode::QueryParam,
            StatusCode::OK,
            &headers,
            2048,
            range_body_length("bytes=2048-3071"),
        ));
    }

    #[test]
    fn rejects_oversized_query_param_ok_response_at_resume_offset() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_LENGTH, HeaderValue::from_static("4096"));

        assert!(!response_matches_requested_offset(
            PreparedRangeMode::QueryParam,
            StatusCode::OK,
            &headers,
            2048,
            range_body_length("bytes=2048-3071"),
        ));
    }

    #[test]
    fn rejects_empty_query_param_ok_response_at_resume_offset() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_LENGTH, HeaderValue::from_static("0"));

        assert!(!response_matches_requested_offset(
            PreparedRangeMode::QueryParam,
            StatusCode::OK,
            &headers,
            2048,
            range_body_length("bytes=2048-3071"),
        ));
    }

    #[tokio::test]
    async fn query_param_range_stream_accepts_ok_response_at_resume_offset() {
        let url = spawn_query_range_responses(vec![("5-9", b"fghij")]);
        let mut request = RangedHttpRequest::new_with_headers(
            reqwest::Client::new(),
            url,
            HeaderMap::new(),
            Some(10),
            5,
            PreparedRangeMode::QueryParam,
        );

        let mut stream = request.create_stream(5).await.unwrap();
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).await.unwrap();

        assert_eq!(bytes, b"fghij");
    }

    #[tokio::test]
    async fn query_param_range_stream_resumes_after_ok_chunk() {
        let url = spawn_query_range_responses(vec![("0-4", b"abcde"), ("5-9", b"fghij")]);
        let mut request = RangedHttpRequest::new_with_headers(
            reqwest::Client::new(),
            url,
            HeaderMap::new(),
            Some(10),
            5,
            PreparedRangeMode::QueryParam,
        );

        let mut first = request.create_stream(0).await.unwrap();
        let mut bytes = Vec::new();
        let error = first.read_to_end(&mut bytes).await.unwrap_err();

        assert_eq!(error.kind(), ErrorKind::UnexpectedEof);
        assert_eq!(bytes, b"abcde");

        let mut second = request.create_stream(5).await.unwrap();
        second.read_to_end(&mut bytes).await.unwrap();

        assert_eq!(bytes, b"abcdefghij");
    }

    fn spawn_query_range_responses(responses: Vec<(&'static str, &'static [u8])>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/videoplayback", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let mut responses = VecDeque::from(responses);
            while let Some((expected_range, body)) = responses.pop_front() {
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

                let request = String::from_utf8_lossy(&request);
                assert!(request.contains(&format!("range={expected_range}")));
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(header.as_bytes()).unwrap();
                stream.write_all(body).unwrap();
            }
        });
        url
    }
}
