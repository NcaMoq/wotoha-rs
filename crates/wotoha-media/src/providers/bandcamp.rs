use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use reqwest::{Client, Url};
use serde_json::Value;
use wotoha_core::{PreparedSource, TrackMetadata, TrackRequest};

use crate::{
    ResolveError,
    html::{decode_html_attribute, extract_attribute, extract_meta_content, extract_script_tag},
    provider::MediaProvider,
};

#[derive(Clone, Debug, Default)]
pub struct BandcampProvider;

#[async_trait]
impl MediaProvider for BandcampProvider {
    fn id(&self) -> &'static str {
        "bandcamp"
    }

    fn supports(&self, raw_url: &str) -> bool {
        let Ok(url) = Url::parse(raw_url) else {
            return false;
        };

        let Some(host) = url.host_str() else {
            return false;
        };

        host.ends_with(".bandcamp.com") && url.path().contains("/track/")
    }

    async fn probe(
        &self,
        raw_url: &str,
        probe_client: &Client,
    ) -> Result<TrackRequest, ResolveError> {
        let page = probe_client
            .get(raw_url)
            .send()
            .await
            .map_err(ResolveError::Request)?
            .error_for_status()
            .map_err(ResolveError::Request)?
            .text()
            .await
            .map_err(ResolveError::Request)?;

        parse_bandcamp_track(raw_url, &page)
    }
}

fn parse_bandcamp_track(raw_url: &str, page: &str) -> Result<TrackRequest, ResolveError> {
    let tralbum = parse_json(&decode_html_attribute(
        &extract_attribute(page, "data-tralbum")
            .ok_or_else(|| ResolveError::Parse("missing Bandcamp data-tralbum".to_owned()))?,
    ))?;
    let ld_json = extract_script_tag(page, r#"<script type="application/ld+json">"#)
        .map(parse_json)
        .transpose()?
        .unwrap_or(Value::Null);

    let current = tralbum
        .get("current")
        .ok_or_else(|| ResolveError::Parse("missing Bandcamp current payload".to_owned()))?;
    let track = tralbum
        .get("trackinfo")
        .and_then(Value::as_array)
        .and_then(|tracks| tracks.first())
        .ok_or_else(|| ResolveError::Parse("missing Bandcamp trackinfo".to_owned()))?;

    let stream_url = track
        .get("file")
        .and_then(|file| file.get("mp3-128"))
        .and_then(Value::as_str)
        .ok_or_else(|| ResolveError::Parse("missing Bandcamp mp3-128 stream URL".to_owned()))?;
    let canonical_url = tralbum
        .get("url")
        .and_then(Value::as_str)
        .or_else(|| json_string_at(&ld_json, &["@id"]))
        .unwrap_or(raw_url);
    let canonical_key = track
        .get("track_id")
        .and_then(Value::as_u64)
        .map(|track_id| format!("bandcamp:track:{track_id}"))
        .unwrap_or_else(|| format!("bandcamp:url:{canonical_url}"));

    let title = current
        .get("title")
        .and_then(Value::as_str)
        .or_else(|| track.get("title").and_then(Value::as_str))
        .unwrap_or(raw_url);
    let author = json_string_at(&ld_json, &["byArtist", "name"])
        .map(str::to_owned)
        .or_else(|| extract_meta_content(page, "property", "og:site_name"))
        .unwrap_or_else(|| "Unknown".to_owned());
    let thumbnail_url = json_string_at(&ld_json, &["image"])
        .map(str::to_owned)
        .or_else(|| extract_meta_content(page, "property", "og:image"));
    let duration = track
        .get("duration")
        .and_then(Value::as_f64)
        .map(Duration::from_secs_f64);

    Ok(TrackRequest::new(
        "bandcamp",
        canonical_key,
        raw_url.to_owned(),
        canonical_url.to_owned(),
        canonical_url.to_owned(),
        PreparedSource::http(
            stream_url.to_owned(),
            Vec::new(),
            None,
            stream_url_expiry(stream_url),
        ),
        TrackMetadata::new(
            title.to_owned(),
            author,
            canonical_url.to_owned(),
            thumbnail_url.map(Arc::<str>::from),
            duration,
        ),
    ))
}

fn parse_json(raw: &str) -> Result<Value, ResolveError> {
    serde_json::from_str(raw).map_err(|error| ResolveError::Parse(error.to_string()))
}

fn json_string_at<'a>(value: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut current = value;
    for segment in path {
        current = current.get(*segment)?;
    }

    current.as_str()
}

fn stream_url_expiry(stream_url: &str) -> Option<u64> {
    Url::parse(stream_url)
        .ok()?
        .query_pairs()
        .find(|(key, _)| key == "ts")
        .and_then(|(_, value)| value.parse::<u64>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bandcamp_track_page_into_prepared_http_source() {
        let html = r#"
        <html>
        <head>
            <meta property="og:site_name" content="Cloudkicker">
            <meta property="og:image" content="https://f4.bcbits.com/img/a3216428789_5.jpg">
            <script type="application/ld+json">
                {"@id":"https://cloudkicker.bandcamp.com/track/94-days","byArtist":{"name":"Cloudkicker"},"image":"https://f4.bcbits.com/img/a3216428789_10.jpg"}
            </script>
        </head>
        <body>
            <script data-tralbum="{&quot;current&quot;:{&quot;title&quot;:&quot;94 Days&quot;},&quot;trackinfo&quot;:[{&quot;track_id&quot;:936441543,&quot;title&quot;:&quot;94 Days&quot;,&quot;file&quot;:{&quot;mp3-128&quot;:&quot;https://t4.bcbits.com/stream/example/mp3-128/936441543?p=0&amp;ts=1777111066&amp;token=abc&quot;},&quot;duration&quot;:341.387}],&quot;url&quot;:&quot;https://cloudkicker.bandcamp.com/track/94-days&quot;}"></script>
        </body>
        </html>
        "#;

        let request =
            parse_bandcamp_track("https://cloudkicker.bandcamp.com/track/94-days", html).unwrap();

        assert_eq!(request.provider_id.as_ref(), "bandcamp");
        assert_eq!(request.canonical_key.as_ref(), "bandcamp:track:936441543");
        assert_eq!(
            request.canonical_url.as_ref(),
            "https://cloudkicker.bandcamp.com/track/94-days"
        );
        assert_eq!(request.metadata.author.as_ref(), "Cloudkicker");

        match request.prepared {
            PreparedSource::Http {
                stream_url,
                expires_at_unix,
                ..
            } => {
                assert!(stream_url.contains("mp3-128/936441543"));
                assert_eq!(expires_at_unix, Some(1777111066));
            }
            other => panic!("expected prepared http source, got {other:?}"),
        }
    }
}
