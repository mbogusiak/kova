//! Headless presentation helpers for HTTP responses: status classes, body kind detection,
//! order-preserving JSON pretty-printing with a preview cap, and size/time labels.

use crate::http_runner::executor::HttpResponseData;

/// Bodies above this size are shown raw and truncated instead of pretty-printed (5 MiB).
pub const PRETTY_PRINT_LIMIT_BYTES: usize = 5 * 1024 * 1024;

/// Semantic class of an HTTP status code, used for the colored status badge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HttpStatusClass {
    /// `1xx`.
    Informational,
    /// `2xx` (green badge).
    Success,
    /// `3xx`.
    Redirection,
    /// `4xx` (orange badge).
    ClientError,
    /// `5xx` (red badge).
    ServerError,
    /// Anything outside `100..=599`.
    Unknown,
}

impl HttpStatusClass {
    /// Classifies `status`.
    #[must_use]
    pub const fn from_status(status: u16) -> Self {
        match status {
            100..=199 => Self::Informational,
            200..=299 => Self::Success,
            300..=399 => Self::Redirection,
            400..=499 => Self::ClientError,
            500..=599 => Self::ServerError,
            _ => Self::Unknown,
        }
    }
}

/// Detected format of a response body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HttpBodyKind {
    /// `application/json`, `+json` or a body that parses as JSON.
    Json,
    /// `application/xml`, `text/xml` or `+xml`.
    Xml,
    /// `text/html`.
    Html,
    /// Any other UTF-8 text.
    Text,
    /// Non-UTF-8 content.
    Binary,
}

/// Text shown in the **Body** tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpBodyPreview {
    /// Detected body format.
    pub kind: HttpBodyKind,
    /// Display text (pretty-printed when `pretty`).
    pub text: String,
    /// `true` when `text` was reformatted.
    pub pretty: bool,
    /// Notice explaining why the preview is partial (size cap, truncated download, binary).
    pub notice: Option<String>,
}

/// Detects the body format from the `Content-Type` header, falling back to sniffing.
#[must_use]
pub fn detect_body_kind(content_type: Option<&str>, body: &[u8]) -> HttpBodyKind {
    let mime = content_type
        .and_then(|ct| ct.split(';').next())
        .map(|m| m.trim().to_ascii_lowercase())
        .unwrap_or_default();
    if mime == "application/json" || mime.ends_with("+json") {
        return HttpBodyKind::Json;
    }
    if mime == "application/xml" || mime == "text/xml" || mime.ends_with("+xml") {
        return HttpBodyKind::Xml;
    }
    if mime == "text/html" {
        return HttpBodyKind::Html;
    }
    let Ok(text) = std::str::from_utf8(body) else {
        return HttpBodyKind::Binary;
    };
    let trimmed = text.trim_start();
    if (trimmed.starts_with('{') || trimmed.starts_with('['))
        && serde_json::from_str::<serde::de::IgnoredAny>(text).is_ok()
    {
        return HttpBodyKind::Json;
    }
    if trimmed.starts_with("<?xml") {
        return HttpBodyKind::Xml;
    }
    HttpBodyKind::Text
}

/// Builds the **Body** tab preview for `response`.
#[must_use]
pub fn body_preview(response: &HttpResponseData) -> HttpBodyPreview {
    body_preview_with_limit(response, PRETTY_PRINT_LIMIT_BYTES)
}

/// Same as [`body_preview`] with an explicit pretty-print cap.
#[must_use]
pub fn body_preview_with_limit(response: &HttpResponseData, limit: usize) -> HttpBodyPreview {
    let kind = detect_body_kind(response.content_type(), &response.body);
    if kind == HttpBodyKind::Binary {
        return HttpBodyPreview {
            kind,
            text: String::new(),
            pretty: false,
            notice: Some(format!(
                "Binary content ({}) is not previewed.",
                format_size(response.size_bytes)
            )),
        };
    }
    let mut notice = response.truncated.then(|| {
        format!(
            "Response truncated: showing {} of {}.",
            format_size(response.body.len() as u64),
            format_size(response.size_bytes)
        )
    });
    if response.body.len() > limit {
        let cut = floor_char_boundary(&response.body, limit);
        notice = Some(format!(
            "Response is {}; pretty-printing is disabled above {}. Showing the first {} raw.",
            format_size(response.size_bytes),
            format_size(limit as u64),
            format_size(cut as u64)
        ));
        return HttpBodyPreview {
            kind,
            text: String::from_utf8_lossy(&response.body[..cut]).into_owned(),
            pretty: false,
            notice,
        };
    }
    let text = String::from_utf8_lossy(&response.body).into_owned();
    if kind == HttpBodyKind::Json && !response.truncated {
        if let Some(pretty) = pretty_print_json(&text) {
            return HttpBodyPreview {
                kind,
                text: pretty,
                pretty: true,
                notice,
            };
        }
    }
    HttpBodyPreview {
        kind,
        text,
        pretty: false,
        notice,
    }
}

fn floor_char_boundary(bytes: &[u8], limit: usize) -> usize {
    let mut cut = limit.min(bytes.len());
    // UTF-8 continuation bytes are 0b10xx_xxxx.
    while cut > 0 && cut < bytes.len() && (bytes[cut] & 0xC0) == 0x80 {
        cut -= 1;
    }
    cut
}

/// Re-indents valid JSON with two spaces, preserving key order and number text exactly.
///
/// Returns `None` if `input` is not valid JSON.
#[must_use]
pub fn pretty_print_json(input: &str) -> Option<String> {
    serde_json::from_str::<serde::de::IgnoredAny>(input).ok()?;
    let mut out = String::with_capacity(input.len() + input.len() / 4);
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '{' | '[' => {
                out.push(c);
                let close = if c == '{' { '}' } else { ']' };
                while chars.peek().is_some_and(|n| n.is_whitespace()) {
                    chars.next();
                }
                if chars.peek() == Some(&close) {
                    chars.next();
                    out.push(close);
                } else {
                    depth += 1;
                    newline_indent(&mut out, depth);
                }
            }
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                newline_indent(&mut out, depth);
                out.push(c);
            }
            ',' => {
                out.push(',');
                newline_indent(&mut out, depth);
            }
            ':' => out.push_str(": "),
            c if c.is_whitespace() => {}
            c => out.push(c),
        }
    }
    Some(out)
}

fn newline_indent(out: &mut String, depth: usize) {
    out.push('\n');
    for _ in 0..depth {
        out.push_str("  ");
    }
}

/// Formats headers as `Name: Value` lines.
#[must_use]
pub fn format_headers(response: &HttpResponseData) -> String {
    let mut out = String::new();
    for header in &response.headers {
        out.push_str(&header.name);
        out.push_str(": ");
        out.push_str(&header.value);
        out.push('\n');
    }
    out
}

/// Formats the status line, e.g. `200 OK`.
#[must_use]
pub fn format_status(response: &HttpResponseData) -> String {
    match &response.reason {
        Some(reason) => format!("{} {reason}", response.status),
        None => response.status.to_string(),
    }
}

/// Formats the **Raw** tab: status line, headers, blank line and the unformatted body.
#[must_use]
pub fn format_raw(response: &HttpResponseData) -> String {
    let limit = floor_char_boundary(&response.body, PRETTY_PRINT_LIMIT_BYTES);
    format!(
        "{} {}\n{}\n{}",
        response.http_version,
        format_status(response),
        format_headers(response),
        String::from_utf8_lossy(&response.body[..limit])
    )
}

/// Formats a byte size with binary units (`512 B`, `1.5 KB`, `2.0 MB`).
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    reason = "sizes are display-only; f64 precision is more than enough"
)]
pub fn format_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let b = bytes as f64;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if b < KIB * KIB {
        format!("{:.1} KB", b / KIB)
    } else if b < KIB * KIB * KIB {
        format!("{:.1} MB", b / (KIB * KIB))
    } else {
        format!("{:.1} GB", b / (KIB * KIB * KIB))
    }
}

/// Formats a duration in milliseconds (`87 ms`, `1.24 s`).
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    reason = "durations are display-only; f64 precision is more than enough"
)]
pub fn format_duration_ms(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms} ms")
    } else {
        format!("{:.2} s", ms as f64 / 1000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http_runner::parser::HttpHeader;

    fn response(content_type: &str, body: &str) -> HttpResponseData {
        HttpResponseData {
            status: 200,
            reason: Some("OK".to_owned()),
            http_version: "HTTP/1.1".to_owned(),
            final_url: "http://x/".to_owned(),
            duration_ms: 5,
            size_bytes: body.len() as u64,
            headers: vec![HttpHeader::new("content-type", content_type)],
            body: body.as_bytes().to_vec(),
            truncated: false,
        }
    }

    #[test]
    fn status_classes() {
        assert_eq!(HttpStatusClass::from_status(204), HttpStatusClass::Success);
        assert_eq!(
            HttpStatusClass::from_status(302),
            HttpStatusClass::Redirection
        );
        assert_eq!(
            HttpStatusClass::from_status(404),
            HttpStatusClass::ClientError
        );
        assert_eq!(
            HttpStatusClass::from_status(503),
            HttpStatusClass::ServerError
        );
        assert_eq!(HttpStatusClass::from_status(42), HttpStatusClass::Unknown);
    }

    #[test]
    fn pretty_prints_json_preserving_order_and_strings() {
        let out = pretty_print_json(r#"{"z":1,"a":[1,2,{}],"s":"a,b:{\"c\"}","e":[]}"#);
        assert_eq!(
            out.as_deref(),
            Some(
                "{\n  \"z\": 1,\n  \"a\": [\n    1,\n    2,\n    {}\n  ],\n  \"s\": \"a,b:{\\\"c\\\"}\",\n  \"e\": []\n}"
            )
        );
        assert!(pretty_print_json("{bad").is_none());
    }

    #[test]
    fn detects_kinds() {
        assert_eq!(
            detect_body_kind(Some("application/problem+json; charset=utf-8"), b"{}"),
            HttpBodyKind::Json
        );
        assert_eq!(
            detect_body_kind(Some("text/plain"), b"[1]"),
            HttpBodyKind::Json
        );
        assert_eq!(
            detect_body_kind(None, b"<?xml version=\"1.0\"?><a/>"),
            HttpBodyKind::Xml
        );
        assert_eq!(
            detect_body_kind(Some("text/html"), b"<p>"),
            HttpBodyKind::Html
        );
        assert_eq!(
            detect_body_kind(None, &[0xff, 0xfe, 0x00]),
            HttpBodyKind::Binary
        );
        assert_eq!(detect_body_kind(None, b"hello"), HttpBodyKind::Text);
    }

    #[test]
    fn preview_pretty_prints_small_json_and_caps_large_bodies() {
        let small = body_preview(&response("application/json", r#"{"a":1}"#));
        assert!(small.pretty);
        assert_eq!(small.text, "{\n  \"a\": 1\n}");
        assert!(small.notice.is_none());

        let large = body_preview_with_limit(&response("application/json", r#"{"a":"żółw"}"#), 9);
        assert!(!large.pretty);
        assert!(large.notice.is_some());
        assert!(large.text.len() <= 9);
    }

    #[test]
    fn labels() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(3 * 1024 * 1024), "3.0 MB");
        assert_eq!(format_duration_ms(87), "87 ms");
        assert_eq!(format_duration_ms(1240), "1.24 s");
        let r = response("text/plain", "hi");
        assert_eq!(format_status(&r), "200 OK");
        assert_eq!(format_headers(&r), "content-type: text/plain\n");
        assert_eq!(
            format_raw(&r),
            "HTTP/1.1 200 OK\ncontent-type: text/plain\n\nhi"
        );
    }
}
