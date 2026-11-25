; Kova IDE Tree-sitter highlight queries for Kotlin (.kt, .kts)
; Every non-internal capture maps 1:1 to `kova_theme::SyntaxTokenKind` via `resolve_capture`.

; Identifiers (lowest priority; overridden by declarations, calls, types, and constants)

(identifier) @variable

; Properties & parameters

(property_declaration
  (variable_declaration
    (identifier) @property))

(navigation_expression
  (identifier) @property .)

(parameter
  (identifier) @variable.parameter)

(class_parameter
  (identifier) @property)

(lambda_parameters
  (variable_declaration
    (identifier) @variable.parameter))

((identifier) @variable.parameter
 (#eq? @variable.parameter "it"))

; Declarations & calls (placed after property navigation so `obj.method()` wins over property)

(function_declaration
  name: (identifier) @function)

(call_expression
  (identifier) @function.call)

(call_expression
  (navigation_expression
    (identifier) @function.call .))

(infix_expression
  (identifier) @function.call)

(constructor_invocation
  (user_type
    (identifier) @constructor))

((call_expression
  (identifier) @constructor)
 (#match? @constructor "^[A-Z]"))

; Types

(user_type
  (identifier) @type)

(class_declaration
  name: (identifier) @type.definition)

(object_declaration
  name: (identifier) @type.definition)

(companion_object
  name: (identifier) @type.definition)

(type_alias
  (identifier) @type.definition)

(type_parameter
  (identifier) @type)

((user_type
  (identifier) @type.builtin)
 (#any-of? @type.builtin
  "Any"
  "Boolean"
  "Byte"
  "Char"
  "Double"
  "Float"
  "Int"
  "Long"
  "Nothing"
  "Short"
  "String"
  "UByte"
  "UInt"
  "ULong"
  "UShort"
  "Unit"))

; Annotations (placed after general types/constructors so annotation names win)

(annotation
  "@" @annotation)

(annotation
  (user_type
    (identifier) @annotation))

(annotation
  (constructor_invocation
    (user_type
      (identifier) @annotation)))

(file_annotation
  "@" @annotation
  "file" @annotation)

(use_site_target) @annotation

; Constants & enum entries

(enum_entry
  (identifier) @constant)

((identifier) @constant
 (#match? @constant "^_*[A-Z][A-Z0-9_]+$"))

; Builtins (`this`, `super`, `true`, `false`, `null`)

[
  (this_expression)
  (super_expression)
] @variable.builtin

((identifier) @boolean
 (#any-of? @boolean "true" "false" "null"))

; Literals

(number_literal) @number
(float_literal) @number.float

[
  (character_literal)
  (string_literal)
  (multiline_string_literal)
] @string

(escape_sequence) @string.escape

; Comments (KDoc comes after generic block_comment so it wins on /** ... */)

[
  (line_comment)
  (block_comment)
  (shebang)
] @comment

((block_comment) @comment.documentation
 (#match? @comment.documentation "^/\\*\\*"))

; Keywords — declarations, modifiers & imports

[
  "package"
  "import"
] @keyword.import

[
  (class_modifier)
  (member_modifier)
  (function_modifier)
  (property_modifier)
  (visibility_modifier)
  (inheritance_modifier)
  (parameter_modifier)
  (platform_modifier)
  (reification_modifier)
  (variance_modifier)
] @keyword.modifier

[
  "class"
  "interface"
  "object"
  "typealias"
  "companion"
] @keyword.type

[
  "fun"
  "val"
  "var"
  "constructor"
  "init"
  "by"
  "where"
  "is"
  "!is"
  "as"
  "as?"
  "in"
  "!in"
  "get"
  "set"
  "dynamic"
] @keyword

; Keywords — control flow

[
  "if"
  "else"
  "when"
] @keyword.conditional

[
  "for"
  "while"
  "do"
] @keyword.repeat

[
  "return"
  "return@"
] @keyword.return

[
  "try"
  "catch"
  "finally"
  "throw"
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
  "=="
  "!="
  "==="
  "!=="
  "<"
  ">"
  "<="
  ">="
  "&&"
  "||"
  "!"
  "!!"
  "?."
  "?:"
  "::"
  "->"
  ".."
  "..<"
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
] @punctuation.delimiter

; Interpolation delimiters placed after brackets so `}` inside `${...}` stays `StringEscape`

(interpolation
  [
    "$"
    "${"
    "}"
  ] @punctuation.special)
