use tracing::info;

use crate::url::summarize_url_for_logs;

pub fn append_debug_log(message: impl AsRef<str>) {
    let message = sanitize_log_message(message.as_ref());
    info!(target: "wotoha_debug", "{message}");
}

pub fn sanitize_log_message(message: &str) -> String {
    let normalized = message
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let mut out = String::with_capacity(normalized.len());
    let mut cursor = 0;

    while let Some(start) = next_url_start(&normalized, cursor) {
        out.push_str(&normalized[cursor..start]);

        let end = normalized[start..]
            .find(char::is_whitespace)
            .map(|offset| start + offset)
            .unwrap_or(normalized.len());
        out.push_str(&sanitize_url_token(&normalized[start..end]));
        cursor = end;
    }

    out.push_str(&normalized[cursor..]);
    out
}

fn next_url_start(message: &str, cursor: usize) -> Option<usize> {
    message[cursor..].char_indices().find_map(|(offset, _)| {
        let start = cursor + offset;
        let rest = &message[start..];
        (rest
            .get(..7)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http://"))
            || rest
                .get(..8)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://")))
        .then_some(start)
    })
}

fn sanitize_url_token(token: &str) -> String {
    let trimmed = token.trim_end_matches([')', ']', '}', ',', ';']);
    let suffix = &token[trimmed.len()..];
    let summary = summarize_url_for_logs(trimmed);
    if summary == "<invalid-url>" {
        format!("<redacted-url>{suffix}")
    } else {
        format!("{summary}{suffix}")
    }
}

#[cfg(test)]
mod tests {
    use super::sanitize_log_message;

    #[test]
    fn strips_sensitive_query_parameters_from_urls() {
        let sanitized = sanitize_log_message(
            "ranged_http: request url=https://rr1---sn.example.com/videoplayback?sig=secret&expire=123&ip=1.2.3.4",
        );

        assert!(sanitized.contains("https://rr1---sn.example.com/[redacted]"));
        assert!(!sanitized.contains("sig=secret"));
        assert!(!sanitized.contains("expire=123"));
        assert!(!sanitized.contains("ip=1.2.3.4"));
    }

    #[test]
    fn strips_sensitive_path_segments_from_urls() {
        let sanitized =
            sanitize_log_message("discord: /play url=https://vimeo.com/76979871/secretshare");

        assert!(sanitized.contains("https://vimeo.com/[redacted]"));
        assert!(!sanitized.contains("secretshare"));
    }

    #[test]
    fn matches_mixed_case_schemes_and_redacts_malformed_urls() {
        let sanitized = sanitize_log_message(
            "request URL=HTTPS://example.test/path?token=secret malformed=HtTp://[not-a-url]",
        );

        assert!(sanitized.contains("https://example.test/[redacted]"));
        assert!(!sanitized.contains("token=secret"));
        assert!(!sanitized.contains("[not-a-url]"));
        assert!(!sanitized.contains("HtTp://"));
    }

    #[test]
    fn replaces_control_characters_before_logging() {
        let sanitized = sanitize_log_message("provider\nsecret\r\u{0000}value");

        assert_eq!(sanitized, "provider secret  value");
    }
}
