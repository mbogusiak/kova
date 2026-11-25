; Kova IDE Tree-sitter fold queries for Java

[
  (class_body)
  (interface_body)
  (enum_body)
  (annotation_type_body)
] @fold.type

(method_declaration
  body: (block)) @fold.function

(constructor_declaration
  body: (constructor_body)) @fold.function

(compact_constructor_declaration
  body: (block)) @fold.function

(static_initializer) @fold.function

[
  (block)
  (switch_block)
  (array_initializer)
] @fold.block

(block_comment) @fold.comment
