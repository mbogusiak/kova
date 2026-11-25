; Kova IDE Tree-sitter entry-point queries for Kotlin (.kt, .kts)
; Matches package headers and concrete functions (with a `function_body`) for `fun main` and `@Test*` detection.

(package_header
  (qualified_identifier) @package.name) @package.decl

(function_declaration
  name: (identifier) @function.name
  (function_value_parameters) @function.params
  (function_body) @function.body) @function.decl
