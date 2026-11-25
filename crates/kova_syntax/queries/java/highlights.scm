; Kova IDE Tree-sitter highlight queries for Java
; Every non-internal capture maps 1:1 to `kova_theme::SyntaxTokenKind` via `resolve_capture`.

; Variables & identifiers (lowest priority; overridden by more specific patterns below)

(identifier) @variable

; Methods & constructors

(method_declaration
  name: (identifier) @function.method)

(method_invocation
  name: (identifier) @function.call)

((method_reference
  (identifier) @function.method)
 (#match? @function.method "^[a-z]"))

(constructor_declaration
  name: (identifier) @constructor)

(compact_constructor_declaration
  name: (identifier) @constructor)

; Fields & parameters

(field_declaration
  declarator: (variable_declarator
    name: (identifier) @property))

(field_access
  field: (identifier) @property)

(formal_parameter
  name: (identifier) @variable.parameter)

(catch_formal_parameter
  name: (identifier) @variable.parameter)

(enhanced_for_statement
  name: (identifier) @variable.parameter)

(spread_parameter
  (variable_declarator
    name: (identifier) @variable.parameter))

(inferred_parameters
  (identifier) @variable.parameter)

(lambda_expression
  parameters: (identifier) @variable.parameter)

; Annotations

"@" @annotation

(annotation
  name: (identifier) @annotation)

(annotation
  name: (scoped_identifier
    name: (identifier) @annotation))

(marker_annotation
  name: (identifier) @annotation)

(marker_annotation
  name: (scoped_identifier
    name: (identifier) @annotation))

; Types

(type_identifier) @type

(scoped_type_identifier
  (type_identifier) @type)

(class_declaration
  name: (identifier) @type.definition)

(interface_declaration
  name: (identifier) @type.definition)

(enum_declaration
  name: (identifier) @type.definition)

(record_declaration
  name: (identifier) @type.definition)

(annotation_type_declaration
  name: (identifier) @type.definition)

((field_access
  object: (identifier) @type)
 (#match? @type "^[A-Z]"))

((scoped_identifier
  scope: (identifier) @type)
 (#match? @type "^[A-Z]"))

((method_invocation
  object: (identifier) @type)
 (#match? @type "^[A-Z]"))

((method_reference
  . (identifier) @type)
 (#match? @type "^[A-Z]"))

[
  (boolean_type)
  (integral_type)
  (floating_point_type)
  (void_type)
] @type.builtin

((type_identifier) @keyword
 (#eq? @keyword "var"))

; Constants & built-in references

(enum_constant
  name: (identifier) @constant)

((identifier) @constant
 (#match? @constant "^_*[A-Z][A-Z0-9_]+$"))

[
  (this)
  (super)
] @variable.builtin

; Literals

[
  (hex_integer_literal)
  (decimal_integer_literal)
  (octal_integer_literal)
  (binary_integer_literal)
] @number

[
  (decimal_floating_point_literal)
  (hex_floating_point_literal)
] @number.float

[
  (character_literal)
  (string_literal)
  (multiline_string_fragment)
] @string

(escape_sequence) @string.escape

(string_interpolation
  [
    "\\{"
    "}"
  ] @punctuation.special)

[
  (true)
  (false)
  (null_literal)
] @boolean

; Comments (Javadoc pattern comes after generic block_comment so it wins on /** ... */)

[
  (line_comment)
  (block_comment)
] @comment

((block_comment) @comment.documentation
 (#match? @comment.documentation "^/\\*\\*"))

; Keywords — declarations, modifiers, imports & modules

[
  "package"
  "import"
  "module"
  "open"
  "opens"
  "exports"
  "requires"
  "transitive"
  "uses"
  "provides"
  "with"
  "to"
] @keyword.import

[
  "class"
  "interface"
  "enum"
  "record"
  "extends"
  "implements"
  "permits"
] @keyword.type

[
  "public"
  "private"
  "protected"
  "static"
  "final"
  "abstract"
  "sealed"
  "non-sealed"
  "synchronized"
  "volatile"
  "transient"
  "native"
  "strictfp"
  "default"
] @keyword.modifier

[
  "new"
  "instanceof"
  "assert"
] @keyword

; Keywords — control flow

[
  "if"
  "else"
  "switch"
  "case"
  "when"
] @keyword.conditional

[
  "for"
  "while"
  "do"
] @keyword.repeat

[
  "break"
  "continue"
  "return"
  "yield"
] @keyword.return

[
  "try"
  "catch"
  "finally"
  "throw"
  "throws"
] @keyword.exception

; Operators & punctuation

[
  "+"
  "-"
  "*"
  "/"
  "%"
  "++"
  "--"
  "="
  "+="
  "-="
  "*="
  "/="
  "%="
  "&="
  "|="
  "^="
  "<<="
  ">>="
  ">>>="
  "=="
  "!="
  "<"
  ">"
  "<="
  ">="
  "&&"
  "||"
  "!"
  "&"
  "|"
  "^"
  "~"
  "<<"
  ">>"
  ">>>"
  "->"
  "::"
  "?"
  ":"
] @operator

[
  "("
  ")"
  "["
  "]"
  "{"
  "}"
] @punctuation.bracket

[
  "."
  ","
  ";"
  "..."
] @punctuation.delimiter
