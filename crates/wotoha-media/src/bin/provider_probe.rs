use std::{sync::Arc, time::Instant};

use tokio::{sync::Semaphore, task::JoinSet};
use wotoha_core::PreparedSource;
use wotoha_media::MediaResolver;

const DEFAULT_CONCURRENCY: usize = 4;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let options = ProbeOptions::parse(std::env::args().skip(1))
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;

    if options.urls.is_empty() {
        eprintln!(
            "usage: cargo run -p wotoha-media --bin provider_probe -- [--warmup] [--prepare] [--concurrency <n>] <url>..."
        );
        std::process::exit(2);
    }

    let resolver = Arc::new(MediaResolver::new()?);
    if options.warmup {
        resolver.warmup_providers().await;
    }
    let slots = Arc::new(Semaphore::new(options.concurrency));
    let mut tasks = JoinSet::new();

    for url in options.urls {
        let resolver = resolver.clone();
        let slots = slots.clone();
        let prepare = options.prepare;
        tasks.spawn(async move {
            let _permit = slots
                .acquire_owned()
                .await
                .expect("provider probe semaphore should stay open");
            render_probe_result(resolver, url, prepare).await
        });
    }

    while let Some(result) = tasks.join_next().await {
        match result {
            Ok(output) => println!("{output}"),
            Err(error) => println!("ERROR\t<task>\t{error}"),
        }
    }

    Ok(())
}

struct ProbeOptions {
    warmup: bool,
    prepare: bool,
    concurrency: usize,
    urls: Vec<String>,
}

impl ProbeOptions {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut warmup = false;
        let mut prepare = false;
        let mut concurrency = DEFAULT_CONCURRENCY;
        let mut urls = Vec::new();
        let mut args = args.into_iter();

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--warmup" => warmup = true,
                "--prepare" => prepare = true,
                "--concurrency" => {
                    let value = args
                        .next()
                        .ok_or_else(|| "--concurrency requires a value".to_owned())?;
                    concurrency = parse_concurrency(&value)?;
                }
                value if value.starts_with("--concurrency=") => {
                    concurrency = parse_concurrency(&value["--concurrency=".len()..])?;
                }
                _ => urls.push(arg),
            }
        }

        Ok(Self {
            warmup,
            prepare,
            concurrency,
            urls,
        })
    }
}

fn parse_concurrency(value: &str) -> Result<usize, String> {
    let parsed = value
        .parse::<usize>()
        .map_err(|_| format!("invalid concurrency: {value}"))?;
    if parsed == 0 {
        return Err("concurrency must be greater than zero".to_owned());
    }
    Ok(parsed)
}

async fn render_probe_result(resolver: Arc<MediaResolver>, url: String, prepare: bool) -> String {
    let started_at = Instant::now();
    match resolver.resolve(&url).await {
        Ok(request) => {
            let prepared = if prepare {
                match resolver.prepare_playback(&request).await {
                    Ok(prepared) => prepared,
                    Err(error) => return format!("ERROR\t{url}\t{error}"),
                }
            } else {
                request.clone()
            };
            let mut output = format!(
                "{}\t{}\t{}\t{}",
                prepared.provider_id,
                prepared.canonical_key,
                prepared.metadata.title,
                prepared.metadata.uri
            );
            match &prepared.prepared {
                PreparedSource::Http {
                    stream_url,
                    expires_at_unix,
                    range_chunk_size,
                    ..
                } => output.push_str(&format!(
                    "\n  source=http\t{}\trange_chunk_size={range_chunk_size:?}\texpires={expires_at_unix:?}",
                    stream_url
                )),
                PreparedSource::Hls {
                    playlist_url,
                    expires_at_unix,
                    ..
                } => output.push_str(&format!(
                    "\n  source=hls\t{}\texpires={expires_at_unix:?}",
                    playlist_url
                )),
            }
            output.push_str(&format!(
                "\n  elapsed_ms={:.2}",
                started_at.elapsed().as_secs_f64() * 1000.0
            ));
            output
        }
        Err(error) => format!(
            "ERROR\t{url}\t{error}\n  elapsed_ms={:.2}",
            started_at.elapsed().as_secs_f64() * 1000.0
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::ProbeOptions;

    #[test]
    fn parses_probe_options() {
        let options = ProbeOptions::parse([
            "--warmup".to_owned(),
            "--prepare".to_owned(),
            "--concurrency=8".to_owned(),
            "https://example.com/a".to_owned(),
        ])
        .unwrap();

        assert!(options.warmup);
        assert!(options.prepare);
        assert_eq!(options.concurrency, 8);
        assert_eq!(options.urls, vec!["https://example.com/a"]);
    }
}
