use reqwest::header::HeaderMap;
use songbird::input::AudioStreamError;
use wotoha_core::url::{is_allowed_prepared_url, same_url_host, summarize_url_for_logs};

pub fn validate_provider_url(provider_id: &str, raw_url: &str) -> Result<(), AudioStreamError> {
    if is_allowed_prepared_url(provider_id, raw_url) {
        Ok(())
    } else {
        Err(AudioStreamError::Fail(
            format!(
                "unsafe HLS child target for provider {provider_id}: {}",
                summarize_url_for_logs(raw_url)
            )
            .into(),
        ))
    }
}

pub fn filtered_headers(origin_url: &str, target_url: &str, headers: &HeaderMap) -> HeaderMap {
    if same_url_host(origin_url, target_url) {
        return headers.clone();
    }

    let mut filtered = HeaderMap::new();
    for (name, value) in headers.iter() {
        if allows_cross_host_header(name.as_str()) {
            filtered.insert(name.clone(), value.clone());
        }
    }
    filtered
}

fn allows_cross_host_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "accept" | "accept-language" | "origin" | "referer" | "user-agent"
    )
}

#[cfg(test)]
mod tests {
    use super::{filtered_headers, validate_provider_url};
    use reqwest::header::{COOKIE, HeaderMap, HeaderValue, ORIGIN, USER_AGENT};

    #[test]
    fn rejects_disallowed_provider_child_urls() {
        let error = validate_provider_url("niconico", "https://example.com/segment.ts");
        assert!(error.is_err());
    }

    #[test]
    fn strips_sensitive_headers_on_cross_host_requests() {
        let mut headers = HeaderMap::new();
        headers.insert(COOKIE, HeaderValue::from_static("user_session=secret"));
        headers.insert(USER_AGENT, HeaderValue::from_static("Mozilla/5.0"));
        headers.insert(ORIGIN, HeaderValue::from_static("https://www.youtube.com"));

        let filtered = filtered_headers(
            "https://www.nicovideo.jp/watch/sm9",
            "https://asset.domand.nicovideo.jp/segment.ts",
            &headers,
        );

        assert!(!filtered.contains_key(COOKIE));
        assert_eq!(
            filtered
                .get(USER_AGENT)
                .and_then(|value| value.to_str().ok()),
            Some("Mozilla/5.0")
        );
        assert_eq!(
            filtered.get(ORIGIN).and_then(|value| value.to_str().ok()),
            Some("https://www.youtube.com")
        );
    }
}
