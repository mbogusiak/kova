; Kova IDE Tree-sitter highlight queries for TOML (.toml)
; Every non-internal capture maps 1:1 to `kova_theme::SyntaxTokenKind` via `resolve_capture`.

; Keys & table headers

(bare_key) @property
(quoted_key) @property

(table
  (bare_key) @property)

(table
  (dotted_key
    (bare_key) @property))

(table_array_element
  (bare_key) @property)

(table_array_element
  (dotted_key
    (bare_key) @property))

; Literals

(string) @string
(escape_sequence) @string.escape

[
  (integer)
  (float)
] @number

[
  (offset_date_time)
  (local_date_time)
  (local_date)
  (local_time)
] @string.special

(boolean) @boolean

(comment) @comment

; Operators & punctuation

"=" @operator

[
  "."
  ","
] @punctuation.delimiter

[
  "["
  "]"
  "[["
  "]]"
  "{"
  "}"
] @punctuation.bracket
