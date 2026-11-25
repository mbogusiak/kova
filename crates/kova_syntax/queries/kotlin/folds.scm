; Kova IDE Tree-sitter fold queries for Kotlin (.kt, .kts)

[
  (class_body)
  (enum_class_body)
] @fold.type

(function_declaration
  (function_body)) @fold.function

(secondary_constructor
  (block)) @fold.function

(anonymous_initializer) @fold.function

[
  (lambda_literal)
  (when_expression)
  (try_expression)
  (block)
] @fold.block

(block_comment) @fold.comment
