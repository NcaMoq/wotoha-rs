use regex::Regex;
use reqwest::{
    Client,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use serde_json::Value;

#[derive(Clone, Copy)]
struct ClientSpec {
    label: &'static str,
    version: &'static str,
    name: &'static str,
    context_json: &'static str,
}

const WEB: ClientSpec = ClientSpec {
    label: "web",
    version: "2.20240726.00.00",
    name: "1",
    context_json: r#""context": {
        "client": {
            "clientName": "WEB",
            "clientVersion": "2.20240726.00.00",
            "hl": "en"
        }
    },"#,
};

const IOS: ClientSpec = ClientSpec {
    label: "ios",
    version: "19.29.1",
    name: "5",
    context_json: r#""context": {
        "client": {
            "clientName": "IOS",
            "clientVersion": "19.29.1",
            "deviceMake": "Apple",
            "deviceModel": "iPhone16,2",
            "userAgent": "com.google.ios.youtube/19.29.1 (iPhone16,2; U; CPU iOS 17_5_1 like Mac OS X;)",
            "osName": "iPhone",
            "osVersion": "17.5.1.21F90",
            "hl": "en"
        }
    },"#,
};

const TV_EMBEDDED: ClientSpec = ClientSpec {
    label: "tv_embedded",
    version: "2.0",
    name: "85",
    context_json: r#""context": {
        "client": {
            "clientName": "TVHTML5_SIMPLY_EMBEDDED_PLAYER",
            "clientVersion": "2.0",
            "hl": "en",
            "clientScreen": "EMBED"
        },
        "thirdParty": {
            "embedUrl": "https://google.com"
        }
    },"#,
};

const YOUTUBE_API_KEY: &str = "AIzaSyAO_FJ2SlqU8Q4STEHLGCilw_Y9_11qcW8";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let url = std::env::args()
        .nth(1)
        .ok_or("usage: youtube_client_probe <youtube-watch-url>")?;
    let client = Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/135.0.0.0 Safari/537.36")
        .build()?;
    let html = client
        .get(&url)
        .query(&[("hl", "en")])
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;

    let video_id = video_id_from_url(&url)?;
    let sts = extract_sts(&html).ok_or("signature timestamp not found")?;
    println!("video_id={video_id}");
    println!("sts={sts}");

    for spec in [WEB, IOS, TV_EMBEDDED] {
        probe_client(&client, spec, &video_id, sts).await?;
    }

    Ok(())
}

async fn probe_client(
    client: &Client,
    spec: ClientSpec,
    video_id: &str,
    sts: u64,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let body = serde_json::from_str::<Value>(&format!(
        r#"{{
            {context}
            "playbackContext": {{
                "contentPlaybackContext": {{
                    "signatureTimestamp": {sts},
                    "html5Preference": "HTML5_PREF_WANTS"
                }}
            }},
            "videoId": "{video_id}"
        }}"#,
        context = spec.context_json,
    ))?;

    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static("content-type"),
        HeaderValue::from_static("application/json"),
    );
    headers.insert(
        HeaderName::from_static("origin"),
        HeaderValue::from_static("https://www.youtube.com"),
    );
    headers.insert(
        HeaderName::from_static("referer"),
        HeaderValue::from_static("https://www.youtube.com/"),
    );
    headers.insert(
        HeaderName::from_static("x-youtube-client-version"),
        HeaderValue::from_str(spec.version)?,
    );
    headers.insert(
        HeaderName::from_static("x-youtube-client-name"),
        HeaderValue::from_str(spec.name)?,
    );

    let response = client
        .post("https://www.youtube.com/youtubei/v1/player")
        .headers(headers)
        .query(&[("key", YOUTUBE_API_KEY)])
        .json(&body)
        .send()
        .await?;

    let status = response.status();
    let text = response.text().await?;
    println!("client={}\tstatus={status}", spec.label);
    let value: Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(_) => {
            println!("  body={text}");
            return Ok(());
        }
    };

    let hls = value
        .pointer("/streamingData/hlsManifestUrl")
        .and_then(Value::as_str)
        .unwrap_or("");
    let dash = value
        .pointer("/streamingData/dashManifestUrl")
        .and_then(Value::as_str)
        .unwrap_or("");
    let adaptive = value
        .pointer("/streamingData/adaptiveFormats")
        .and_then(Value::as_array)
        .map(|entries| entries.len())
        .unwrap_or_default();
    let formats = value
        .pointer("/streamingData/formats")
        .and_then(Value::as_array)
        .map(|entries| entries.len())
        .unwrap_or_default();
    println!("  hls={}", !hls.is_empty());
    println!("  dash={}", !dash.is_empty());
    println!("  formats={formats} adaptive={adaptive}");

    if let Some(entries) = value
        .pointer("/streamingData/adaptiveFormats")
        .and_then(Value::as_array)
    {
        for entry in entries.iter().take(5) {
            let itag = entry
                .get("itag")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            let mime = entry
                .get("mimeType")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let has_url = entry.get("url").and_then(Value::as_str).is_some();
            let has_cipher = entry
                .get("signatureCipher")
                .and_then(Value::as_str)
                .is_some()
                || entry.get("cipher").and_then(Value::as_str).is_some();
            println!(
                "  adaptive\titag={itag}\thas_url={has_url}\thas_cipher={has_cipher}\tmime={mime}"
            );
        }
    }

    Ok(())
}

fn video_id_from_url(url: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let parsed = reqwest::Url::parse(url)?;
    if let Some(host) = parsed.host_str()
        && host.eq_ignore_ascii_case("youtu.be")
    {
        return Ok(parsed
            .path_segments()
            .and_then(|mut segments| segments.next())
            .ok_or("missing youtu.be path")?
            .to_owned());
    }

    parsed
        .query_pairs()
        .find(|(key, _)| key == "v")
        .map(|(_, value)| value.into_owned())
        .ok_or_else(|| "missing youtube video id".into())
}

fn extract_sts(html: &str) -> Option<u64> {
    let regex = Regex::new(r#""sts":(\d+)|"STS":(\d+)"#).ok()?;
    let captures = regex.captures(html)?;
    captures
        .get(1)
        .or_else(|| captures.get(2))
        .and_then(|m| m.as_str().parse().ok())
}
