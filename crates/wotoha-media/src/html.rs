pub fn extract_attribute(document: &str, attribute: &str) -> Option<String> {
    find_attribute_value(document, attribute).map(str::to_owned)
}

pub fn extract_meta_content(document: &str, key: &str, value: &str) -> Option<String> {
    let mut cursor = 0;
    while let Some(offset) = document[cursor..].find("<meta") {
        let start = cursor + offset;
        let end = document[start..].find('>')? + start;
        let tag = &document[start..=end];
        if find_attribute_value(tag, key).is_some_and(|found| found == value) {
            return find_attribute_value(tag, "content").map(str::to_owned);
        }
        cursor = end + 1;
    }

    None
}

pub fn extract_script_tag<'a>(document: &'a str, open_tag: &str) -> Option<&'a str> {
    if let Some(start) = document.find(open_tag) {
        let start = start + open_tag.len();
        let tail = &document[start..];
        let end = tail.find("</script>")?;
        return Some(tail[..end].trim());
    }

    if !open_tag.contains("application/ld+json") {
        return None;
    }

    let mut cursor = 0;
    while let Some(offset) = document[cursor..].find("<script") {
        let start = cursor + offset;
        let tag_end = document[start..].find('>')? + start;
        let tag = &document[start..=tag_end];
        let content_start = tag_end + 1;
        if find_attribute_value(tag, "type")
            .is_some_and(|found| found.eq_ignore_ascii_case("application/ld+json"))
        {
            let tail = &document[content_start..];
            let end = tail.find("</script>")?;
            return Some(tail[..end].trim());
        }
        cursor = content_start;
    }

    None
}

pub fn decode_html_attribute(value: &str) -> String {
    value
        .replace("&quot;", "\"")
        .replace("&#34;", "\"")
        .replace("&amp;", "&")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

fn find_attribute_value<'a>(document: &'a str, attribute: &str) -> Option<&'a str> {
    let mut cursor = 0;
    while let Some(offset) = document[cursor..].find(attribute) {
        let start = cursor + offset;
        if !attribute_boundary_before(document, start) {
            cursor = start + attribute.len();
            continue;
        }

        let mut tail = document[start + attribute.len()..].trim_start();
        if !tail.starts_with('=') {
            cursor = start + attribute.len();
            continue;
        }

        tail = tail[1..].trim_start();
        let quote = tail.chars().next()?;
        if quote != '"' && quote != '\'' {
            cursor = start + attribute.len();
            continue;
        }

        let value_tail = &tail[quote.len_utf8()..];
        let end = value_tail.find(quote)?;
        return Some(&value_tail[..end]);
    }

    None
}

fn attribute_boundary_before(document: &str, start: usize) -> bool {
    document[..start]
        .chars()
        .next_back()
        .is_none_or(|ch| !matches!(ch, 'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | ':'))
}

#[cfg(test)]
mod tests {
    use super::{extract_attribute, extract_meta_content, extract_script_tag};

    #[test]
    fn extracts_attribute_with_single_quotes_and_spaces() {
        let html = "<script class='player' data-tralbum = '{&quot;id&quot;:1}'></script>";

        assert_eq!(
            extract_attribute(html, "data-tralbum").as_deref(),
            Some("{&quot;id&quot;:1}")
        );
    }

    #[test]
    fn extracts_meta_content_regardless_of_attribute_order() {
        let html = r#"<meta content="payload" name="server-response">"#;

        assert_eq!(
            extract_meta_content(html, "name", "server-response").as_deref(),
            Some("payload")
        );
    }

    #[test]
    fn extracts_json_ld_script_with_extra_attributes() {
        let html = r#"<script defer type='application/ld+json'>{"name":"track"}</script>"#;

        assert_eq!(
            extract_script_tag(html, r#"<script type="application/ld+json">"#),
            Some(r#"{"name":"track"}"#)
        );
    }
}
