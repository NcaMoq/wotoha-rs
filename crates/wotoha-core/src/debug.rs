use tracing::info;

use crate::url::summarize_url_for_logs;

pub fn append_debug_log(message: impl AsRef<str>) {
    let message = sanitize_log_message(message.as_ref());
    info!(target: "wotoha_debug", "{message}");
}

pub fn sanitize_log_message(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    let mut cursor = 0;

    while let Some(start) = next_url_start(message, cursor) {
        out.push_str(&message[cursor..start]);

        let end = message[start..]
            .find(char::is_whitespace)
            .map(|offset| start + offset)
            .unwrap_or(message.len());
        out.push_str(&sanitize_url_token(&message[start..end]));
        cursor = end;
    }

    out.push_str(&message[cursor..]);
    out
}

fn next_url_start(message: &str, cursor: usize) -> Option<usize> {
    let http = message[cursor..]
        .find("http://")
        .map(|offset| cursor + offset);
    let https = message[cursor..]
        .find("https://")
        .map(|offset| cursor + offset);
    match (http, https) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(found), None) | (None, Some(found)) => Some(found),
        (None, None) => None,
    }
}

fn sanitize_url_token(token: &str) -> String {
    let trimmed = token.trim_end_matches([')', ']', '}', ',', ';']);
    let suffix = &token[trimmed.len()..];
    let summary = summarize_url_for_logs(trimmed);
    if summary == "<invalid-url>" {
        token.to_owned()
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
}
