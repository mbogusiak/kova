//! Line-based parser for `.http` request files (`IntelliJ` HTTP Client format).
//!
//! Supported syntax:
//! - requests separated by lines starting with `###` (the rest of the line is the request name);
//! - `#` / `//` comment lines and `# @name <name>` request names before the request line;
//! - file-level variables `@name = value` before a request line;
//! - request line `METHOD URL [HTTP/x.y]`, with indented `?`/`&` continuation lines;
//! - `Name: Value` headers until the first blank line, then the request body;
//! - response-handler (`> ...`) and response-reference (`<> ...`) lines are ignored.

use std::collections::BTreeMap;
use std::ops::RangeInclusive;

/// HTTP methods recognized on a request line (kept in sync with `kova_syntax` entry points).
pub const SUPPORTED_METHODS: [&str; 9] = [
    "GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS", "TRACE", "CONNECT",
];

/// Single `Name: Value` header as written in a request (before variable substitution).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpHeader {
    /// Header name (trimmed).
    pub name: String,
    /// Header value (trimmed).
    pub value: String,
}

impl HttpHeader {
    /// Creates a header from `name` and `value`.
    #[must_use]
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }
}

/// One request block parsed from an `.http` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequestSpec {
    /// 0-based index of this request within the file.
    pub index: usize,
    /// Optional request name (`### name` or `# @name name`).
    pub name: Option<String>,
    /// Uppercase HTTP method.
    pub method: String,
    /// Raw URL (may contain `{{var}}` placeholders).
    pub url: String,
    /// Optional explicit protocol version (`HTTP/1.1`).
    pub http_version: Option<String>,
    /// Headers in declaration order.
    pub headers: Vec<HttpHeader>,
    /// Optional raw body (may contain `{{var}}` placeholders).
    pub body: Option<String>,
    /// 0-based line of the request line (`METHOD URL`) — the gutter `▶` line.
    pub request_line: usize,
    /// Inclusive 0-based line range from the request line to the last non-blank body line.
    pub line_range: RangeInclusive<usize>,
}

impl HttpRequestSpec {
    /// Short label such as `GET {{baseUrl}}/users`.
    #[must_use]
    pub fn summary(&self) -> String {
        format!("{} {}", self.method, self.url)
    }
}

/// Parsed `.http` file: requests plus file-level `@name = value` variables.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HttpFile {
    /// Requests in file order.
    pub requests: Vec<HttpRequestSpec>,
    /// File-level variables declared with `@name = value`.
    pub variables: BTreeMap<String, String>,
}

impl HttpFile {
    /// Parses the full text of an `.http` file.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        parse_http_file(text)
    }

    /// Returns the request whose `request_line` is `line`, or otherwise whose
    /// [`HttpRequestSpec::line_range`] contains `line`.
    #[must_use]
    pub fn request_at_line(&self, line: usize) -> Option<&HttpRequestSpec> {
        self.requests
            .iter()
            .find(|req| req.request_line == line)
            .or_else(|| {
                self.requests
                    .iter()
                    .find(|req| req.line_range.contains(&line))
            })
    }
}

/// Parses the full text of an `.http` file into requests and file-level variables.
#[must_use]
pub fn parse_http_file(text: &str) -> HttpFile {
    let lines: Vec<&str> = text
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();

    let mut file = HttpFile::default();
    let mut block_start = 0usize;
    let mut block_name: Option<String> = None;

    for (row, line) in lines.iter().enumerate() {
        if let Some(rest) = line.trim_start().strip_prefix("###") {
            parse_block(&lines, block_start..row, block_name.take(), &mut file);
            let title = rest.trim();
            block_name = (!title.is_empty()).then(|| title.to_owned());
            block_start = row + 1;
        }
    }
    parse_block(&lines, block_start..lines.len(), block_name, &mut file);
    file
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Preamble,
    Headers,
    Body,
}

fn parse_block(
    lines: &[&str],
    rows: std::ops::Range<usize>,
    separator_name: Option<String>,
    file: &mut HttpFile,
) {
    let mut name = separator_name;
    let mut section = Section::Preamble;
    let mut method = String::new();
    let mut url = String::new();
    let mut http_version = None;
    let mut request_line = 0usize;
    let mut last_content_line = 0usize;
    let mut headers = Vec::new();
    let mut body_lines: Vec<&str> = Vec::new();

    for row in rows {
        let line = lines[row];
        let trimmed = line.trim();
        match section {
            Section::Preamble => {
                if trimmed.is_empty() {
                    continue;
                }
                if let Some(comment) = comment_body(trimmed) {
                    if let Some(tag) = comment.strip_prefix("@name") {
                        let tag = tag.trim();
                        if !tag.is_empty() {
                            name = Some(tag.to_owned());
                        }
                    }
                    continue;
                }
                if let Some((var, value)) = parse_variable_declaration(trimmed) {
                    file.variables.insert(var, value);
                    continue;
                }
                let Some((m, u, v)) = parse_request_line(trimmed) else {
                    continue;
                };
                method = m;
                url = u;
                http_version = v;
                request_line = row;
                last_content_line = row;
                section = Section::Headers;
            }
            Section::Headers => {
                if trimmed.is_empty() {
                    section = Section::Body;
                    continue;
                }
                if line.starts_with(char::is_whitespace)
                    && (trimmed.starts_with('?') || trimmed.starts_with('&'))
                {
                    url.push_str(trimmed);
                    last_content_line = row;
                    continue;
                }
                if comment_body(trimmed).is_some() {
                    continue;
                }
                if let Some((h_name, h_value)) = trimmed.split_once(':') {
                    let h_name = h_name.trim();
                    if !h_name.is_empty() && !h_name.contains(char::is_whitespace) {
                        headers.push(HttpHeader::new(h_name, h_value.trim()));
                        last_content_line = row;
                        continue;
                    }
                }
                // Not a header: treat as the first body line (body without a blank separator).
                section = Section::Body;
                body_lines.push(line);
                last_content_line = row;
            }
            Section::Body => {
                if trimmed.starts_with("> ") || trimmed.starts_with("<> ") || trimmed == ">" {
                    continue;
                }
                body_lines.push(line);
                if !trimmed.is_empty() {
                    last_content_line = row;
                }
            }
        }
    }

    if method.is_empty() {
        return;
    }

    while body_lines.last().is_some_and(|l| l.trim().is_empty()) {
        body_lines.pop();
    }
    let body = (!body_lines.is_empty()).then(|| body_lines.join("\n"));

    file.requests.push(HttpRequestSpec {
        index: file.requests.len(),
        name,
        method,
        url,
        http_version,
        headers,
        body,
        request_line,
        line_range: request_line..=last_content_line,
    });
}

fn comment_body(trimmed: &str) -> Option<&str> {
    trimmed
        .strip_prefix('#')
        .or_else(|| trimmed.strip_prefix("//"))
        .map(str::trim)
}

fn parse_variable_declaration(trimmed: &str) -> Option<(String, String)> {
    let rest = trimmed.strip_prefix('@')?;
    let (name, value) = rest.split_once('=')?;
    let name = name.trim();
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
    {
        return None;
    }
    Some((name.to_owned(), value.trim().to_owned()))
}

fn parse_request_line(trimmed: &str) -> Option<(String, String, Option<String>)> {
    let mut parts = trimmed.split_whitespace();
    let method = parts.next()?.to_ascii_uppercase();
    if !SUPPORTED_METHODS.contains(&method.as_str()) {
        return None;
    }
    let rest: Vec<&str> = parts.collect();
    let (url_parts, version) = match rest.split_last() {
        Some((last, head)) if last.starts_with("HTTP/") && !head.is_empty() => {
            (head, Some((*last).to_owned()))
        }
        _ => (rest.as_slice(), None),
    };
    let url = url_parts.join(" ");
    if url.is_empty() {
        return None;
    }
    Some((method, url, version))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_get_without_separator() {
        let file = parse_http_file("GET https://example.com/api\n");
        assert_eq!(file.requests.len(), 1);
        let req = &file.requests[0];
        assert_eq!(req.method, "GET");
        assert_eq!(req.url, "https://example.com/api");
        assert_eq!(req.request_line, 0);
        assert_eq!(req.line_range, 0..=0);
        assert!(req.body.is_none());
    }

    #[test]
    fn parses_version_headers_and_body_with_crlf() {
        let text = "POST {{baseUrl}}/orders HTTP/1.1\r\nContent-Type: application/json\r\nAuthorization: Bearer {{token}}\r\n\r\n{\r\n  \"id\": 1\r\n}\r\n\r\n";
        let file = parse_http_file(text);
        let req = &file.requests[0];
        assert_eq!(req.method, "POST");
        assert_eq!(req.url, "{{baseUrl}}/orders");
        assert_eq!(req.http_version.as_deref(), Some("HTTP/1.1"));
        assert_eq!(
            req.headers,
            vec![
                HttpHeader::new("Content-Type", "application/json"),
                HttpHeader::new("Authorization", "Bearer {{token}}"),
            ]
        );
        assert_eq!(req.body.as_deref(), Some("{\n  \"id\": 1\n}"));
        assert_eq!(req.line_range, 0..=6);
    }

    #[test]
    fn query_continuation_lines_extend_url() {
        let file =
            parse_http_file("GET https://x.dev/search\n    ?q=kova\n    &page=2\nAccept: */*\n");
        let req = &file.requests[0];
        assert_eq!(req.url, "https://x.dev/search?q=kova&page=2");
        assert_eq!(req.headers.len(), 1);
    }

    #[test]
    fn ignores_response_handlers_and_unknown_lines() {
        let text = "GET https://x.dev\n\n> {% client.test(); %}\n<> ./previous.json\n";
        let req = &parse_http_file(text).requests[0];
        assert!(req.body.is_none());
    }

    #[test]
    fn names_from_separator_and_tag() {
        let text = "### List users\nGET https://x.dev/users\n\n###\n# @name createUser\nPOST https://x.dev/users\n";
        let file = parse_http_file(text);
        assert_eq!(file.requests[0].name.as_deref(), Some("List users"));
        assert_eq!(file.requests[1].name.as_deref(), Some("createUser"));
        assert_eq!(file.requests[1].request_line, 5);
    }

    #[test]
    fn collects_file_variables_and_skips_empty_blocks() {
        let text = "@host = localhost:8080\n###\n\n### only comment\n# nothing\n###\nget http://{{host}}/a\n";
        let file = parse_http_file(text);
        assert_eq!(
            file.variables.get("host").map(String::as_str),
            Some("localhost:8080")
        );
        assert_eq!(file.requests.len(), 1);
        assert_eq!(file.requests[0].method, "GET");
        assert_eq!(file.requests[0].index, 0);
    }

    #[test]
    fn request_at_line_matches_request_line_and_range() {
        let text = "GET https://a.dev\nAccept: */*\n\n###\nPOST https://b.dev\n\nbody\n";
        let file = parse_http_file(text);
        assert_eq!(file.request_at_line(0).map(|r| r.index), Some(0));
        assert_eq!(file.request_at_line(1).map(|r| r.index), Some(0));
        assert_eq!(file.request_at_line(4).map(|r| r.index), Some(1));
        assert_eq!(file.request_at_line(6).map(|r| r.index), Some(1));
        assert!(file.request_at_line(3).is_none());
    }
}
