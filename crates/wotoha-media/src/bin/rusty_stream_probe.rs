use std::time::Duration;

use rusty_ytdl::{Video, VideoOptions, VideoQuality, VideoSearchOptions};

const DEFAULT_MAX_CHUNKS: usize = 8;
const DEFAULT_MAX_BYTES: usize = 8 * 1024 * 1024;
const DEFAULT_CHUNK_TIMEOUT: Duration = Duration::from_secs(15);

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let options = StreamProbeOptions::parse(std::env::args().skip(1))
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    let client = reqwest::Client::builder()
        .user_agent("wotoha-rust/0.1.0")
        .build()?;
    let video_options = VideoOptions {
        quality: VideoQuality::HighestAudio,
        filter: VideoSearchOptions::Audio,
        request_options: rusty_ytdl::RequestOptions {
            client: Some(client),
            ..Default::default()
        },
        ..Default::default()
    };

    let video = Video::new_with_options(options.url.clone(), video_options)?;
    let info = video.get_info().await?;
    println!(
        "info formats={} hls={} dash={}",
        info.formats.len(),
        info.hls_manifest_url.is_some(),
        info.dash_manifest_url.is_some()
    );
    for format in info.formats.iter().take(12) {
        println!(
            "format itag={} mime={} has_url={} has_audio={} has_video={} is_hls={} url={}",
            format.itag,
            format.mime_type.mime,
            !format.url.is_empty(),
            format.has_audio,
            format.has_video,
            format.is_hls,
            format.url
        );
    }
    let stream = video.stream().await?;
    let mut count = 0usize;
    let mut total = 0usize;
    while count < options.max_chunks && total < options.max_bytes {
        let chunk = tokio::time::timeout(options.chunk_timeout, stream.chunk()).await??;
        let Some(chunk) = chunk else {
            break;
        };
        count += 1;
        total += chunk.len();
        println!("chunk[{count}]={}", chunk.len());
    }
    println!(
        "done chunks={count} total={total} capped={}",
        count >= options.max_chunks || total >= options.max_bytes
    );
    Ok(())
}

struct StreamProbeOptions {
    url: String,
    max_chunks: usize,
    max_bytes: usize,
    chunk_timeout: Duration,
}

impl StreamProbeOptions {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut max_chunks = DEFAULT_MAX_CHUNKS;
        let mut max_bytes = DEFAULT_MAX_BYTES;
        let mut chunk_timeout = DEFAULT_CHUNK_TIMEOUT;
        let mut url = None;
        let mut args = args.into_iter();

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--max-chunks" => {
                    let value = args
                        .next()
                        .ok_or_else(|| "--max-chunks requires a value".to_owned())?;
                    max_chunks = parse_positive_usize("--max-chunks", &value)?;
                }
                "--max-bytes" => {
                    let value = args
                        .next()
                        .ok_or_else(|| "--max-bytes requires a value".to_owned())?;
                    max_bytes = parse_positive_usize("--max-bytes", &value)?;
                }
                "--timeout-secs" => {
                    let value = args
                        .next()
                        .ok_or_else(|| "--timeout-secs requires a value".to_owned())?;
                    chunk_timeout =
                        Duration::from_secs(parse_positive_usize("--timeout-secs", &value)? as u64);
                }
                value if value.starts_with("--max-chunks=") => {
                    max_chunks =
                        parse_positive_usize("--max-chunks", &value["--max-chunks=".len()..])?;
                }
                value if value.starts_with("--max-bytes=") => {
                    max_bytes =
                        parse_positive_usize("--max-bytes", &value["--max-bytes=".len()..])?;
                }
                value if value.starts_with("--timeout-secs=") => {
                    chunk_timeout = Duration::from_secs(parse_positive_usize(
                        "--timeout-secs",
                        &value["--timeout-secs=".len()..],
                    )? as u64);
                }
                _ if url.is_none() => url = Some(arg),
                _ => return Err(format!("unexpected argument: {arg}")),
            }
        }

        Ok(Self {
            url: url.ok_or_else(|| {
                "usage: rusty_stream_probe [--max-chunks <n>] [--max-bytes <n>] [--timeout-secs <n>] <url>"
                    .to_owned()
            })?,
            max_chunks,
            max_bytes,
            chunk_timeout,
        })
    }
}

fn parse_positive_usize(name: &str, value: &str) -> Result<usize, String> {
    let parsed = value
        .parse::<usize>()
        .map_err(|_| format!("{name} must be a positive integer"))?;
    if parsed == 0 {
        return Err(format!("{name} must be greater than zero"));
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_MAX_BYTES, StreamProbeOptions};

    #[test]
    fn parses_stream_probe_limits() {
        let options = StreamProbeOptions::parse([
            "--max-chunks=2".to_owned(),
            "--max-bytes".to_owned(),
            "4096".to_owned(),
            "--timeout-secs=3".to_owned(),
            "https://example.com/watch?v=1".to_owned(),
        ])
        .unwrap();

        assert_eq!(options.max_chunks, 2);
        assert_eq!(options.max_bytes, 4096);
        assert_eq!(options.chunk_timeout.as_secs(), 3);
        assert_eq!(options.url, "https://example.com/watch?v=1");
    }

    #[test]
    fn stream_probe_uses_bounded_defaults() {
        let options = StreamProbeOptions::parse(["https://example.com".to_owned()]).unwrap();

        assert_eq!(options.max_chunks, 8);
        assert_eq!(options.max_bytes, DEFAULT_MAX_BYTES);
    }
}
