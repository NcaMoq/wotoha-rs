use std::time::Instant;

use songbird::Songbird;
use wotoha_media::MediaResolver;
use wotoha_runtime::SongbirdRuntime;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let options = ProbeOptions::parse(std::env::args().skip(1))
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    if options.urls.is_empty() {
        eprintln!("usage: cargo run -p wotoha-app --bin track_probe -- [--warmup] <url>...");
        std::process::exit(2);
    }

    let resolver = MediaResolver::new()?;
    if options.warmup {
        resolver.warmup_providers().await;
    }
    let runtime = SongbirdRuntime::new(Songbird::serenity())?;

    for url in options.urls {
        let started_at = Instant::now();
        println!("SOURCE\t{url}");
        match resolver.resolve(&url).await {
            Ok(request) => {
                let prepared = resolver.prepare_playback(&request).await?;
                println!(
                    "RESOLVED\t{}\t{}\t{}",
                    prepared.provider_id, prepared.canonical_key, prepared.metadata.title
                );
                match runtime.verify_track(&prepared).await {
                    Ok(()) => println!("PLAYABLE\tok"),
                    Err(error) => println!("PLAYABLE\terror\t{error}"),
                }
            }
            Err(error) => println!("RESOLVED\terror\t{error}"),
        }
        println!(
            "ELAPSED_MS\t{:.2}",
            started_at.elapsed().as_secs_f64() * 1000.0
        );
    }

    Ok(())
}

struct ProbeOptions {
    warmup: bool,
    urls: Vec<String>,
}

impl ProbeOptions {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut warmup = false;
        let mut urls = Vec::new();

        for arg in args {
            match arg.as_str() {
                "--warmup" => warmup = true,
                value if value.starts_with("--") => {
                    return Err(format!("unknown option: {value}"));
                }
                _ => urls.push(arg),
            }
        }

        Ok(Self { warmup, urls })
    }
}
