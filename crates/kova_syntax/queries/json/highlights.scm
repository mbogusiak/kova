; Kova IDE Tree-sitter highlight queries for JSON (.json)
; Every non-internal capture maps 1:1 to `kova_theme::SyntaxTokenKind` via `resolve_capture`.

; Values & literals (property key rule below wins over `(string) @string` on keys)

(string) @string
(escape_sequence) @string.escape

(pair
  key: (string) @property)

(number) @number

[
  (true)
  (false)
] @boolean

(null) @null

(comment) @comment

; Punctuation

[
  "{"
  "}"
  "["
  "]"
] @punctuation.bracket

[
  ":"
  ","
] @punctuation.delimiter
