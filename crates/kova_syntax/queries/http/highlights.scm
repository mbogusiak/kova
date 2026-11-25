; Kova IDE Tree-sitter highlight queries for HTTP request files (.http)
; Every non-internal capture maps 1:1 to `kova_theme::SyntaxTokenKind` via `resolve_capture`.

; Comments & section separators (`#`, `//`, `###`)

[
  (comment)
  (request_separator)
] @comment

; Request line: method, target URL, HTTP protocol version

(method) @keyword.http.method

(target_url) @string.special.url

(http_version) @constant.http.version

; Response status line

(status_code) @number.http.status
(status_text) @constant.http.version

; Headers

(header
  name: (header_entity) @property.http.header)

(header
  value: (value) @string)

; Variables (`@baseUrl = ...` and `{{baseUrl}}`)

(variable_declaration
  name: (identifier) @variable
  value: (value) @string)

(variable
  name: (identifier) @variable)

; Request & response bodies / external redirects

[
  (json_body)
  (xml_body)
  (graphql_data)
  (raw_body)
  (path)
] @string

; Operators & punctuation

"=" @operator

[
  "{{"
  "}}"
  "{%"
  "%}"
] @punctuation.special

[
  ":"
  "@"
  "<"
  ">"
  "--"
] @punctuation.delimiter
